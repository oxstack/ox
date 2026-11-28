use super::{
    Error, MAX_WIRE_BYTES, Result,
    sse::{SseEvent, SseFramer},
};
use reqwest::{blocking::Response, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::VecDeque, io::Read as _};

#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub name: String,
    pub properties: Value,
}

/// A stream can observe one session or its whole owner route. The native server is volatile and
/// does not replay missed events, so no upstream cursor is sent.
pub struct Events {
    input: Response,
    session: Option<String>,
    framer: SseFramer,
    queued: VecDeque<SseEvent>,
}

impl Events {
    pub(super) fn new(response: Response, session: Option<String>) -> Result<Self> {
        if response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|header| header.to_str().ok())
            .and_then(|header| header.split(';').next())
            != Some("text/event-stream")
        {
            return Err(Error::InvalidResponse(
                "OpenCode event stream has the wrong content type",
            ));
        }
        Ok(Self {
            input: response,
            session,
            framer: SseFramer::new(MAX_WIRE_BYTES),
            queued: VecDeque::new(),
        })
    }

    /// `None` signals a clean stream end, which still requires reconciliation
    /// when reconnecting. An incomplete frame is never emitted.
    pub fn read_event(&mut self) -> Result<Option<Event>> {
        let mut chunk = [0; 8192];
        loop {
            if let Some(raw) = self.queued.pop_front() {
                let event: Event = serde_json::from_str(&raw.data)?;
                if self
                    .session
                    .as_ref()
                    .is_none_or(|id| event.properties["sessionID"] == *id)
                    || matches!(
                        event.name.as_str(),
                        "server.connected" | "server.heartbeat" | "server.instance.disposed"
                    )
                {
                    return Ok(Some(event));
                }
                continue;
            }
            let size = self.input.read(&mut chunk)?;
            if size == 0 {
                self.framer.finish().map_err(|_| {
                    Error::InvalidResponse("OpenCode event stream ended within a frame")
                })?;
                return Ok(None);
            }
            self.queued = self
                .framer
                .feed(&chunk[..size])
                .map_err(|_| {
                    Error::InvalidResponse(
                        "OpenCode event has invalid framing or exceeds the host transport limit",
                    )
                })?
                .into();
        }
    }
}
