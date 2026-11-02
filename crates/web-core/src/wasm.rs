//! JavaScript bindings. Calls return nothing; events queue up and the page
//! drains them with `nextEvent`.

use crate::{filexfer::FileEvent, reconnect, Event, ViewerSession};
use protocol::clipboard::ClipboardContent;
use std::collections::VecDeque;
use std::time::Duration;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Viewer {
    session: ViewerSession,
    events: VecDeque<Event>,
}

/// One queued event. `kind` is "send", "authenticated", "video", "monitors"
/// or "ended"; `bytes` is set for send and video, `text` for monitors and ended.
#[wasm_bindgen]
pub struct ViewerEvent {
    kind: &'static str,
    bytes: Vec<u8>,
    text: String,
}

#[wasm_bindgen]
impl ViewerEvent {
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        self.kind.to_owned()
    }

    /// Moves the bytes out; a second call returns an empty array.
    #[wasm_bindgen(js_name = takeBytes)]
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    #[wasm_bindgen(getter)]
    pub fn text(&self) -> String {
        self.text.clone()
    }
}

impl From<Event> for ViewerEvent {
    fn from(event: Event) -> Self {
        let (kind, bytes, text) = match event {
            Event::Send(bytes) => ("send", bytes, String::new()),
            Event::Authenticated => ("authenticated", Vec::new(), String::new()),
            Event::Video(bytes) => ("video", bytes, String::new()),
            // `text` carries the channel count so the page can configure audio.
            Event::Audio(payload, channels) => ("audio", payload, channels.to_string()),
            Event::Monitors(json) => ("monitors", Vec::new(), json),
            Event::Clipboard(ClipboardContent::Text(text)) => ("clipboard-text", Vec::new(), text),
            Event::Clipboard(ClipboardContent::Image { width, height, png }) => {
                ("clipboard-image", png, format!("{width}x{height}"))
            }
            Event::File(file) => return file.into(),
            Event::Ended(message) => ("ended", Vec::new(), message),
        };
        Self { kind, bytes, text }
    }
}

impl From<FileEvent> for ViewerEvent {
    fn from(event: FileEvent) -> Self {
        // `text` carries a small JSON record; `bytes` carries received data.
        let (kind, bytes, text) = match event {
            FileEvent::Offer { id, name, size } => (
                "file-offer",
                Vec::new(),
                serde_json::json!({ "id": id, "name": name, "size": size }).to_string(),
            ),
            FileEvent::Progress { id, transferred, total, sending } => (
                "file-progress",
                Vec::new(),
                serde_json::json!({ "id": id, "transferred": transferred, "total": total, "sending": sending }).to_string(),
            ),
            FileEvent::Received { id, name, data } => (
                "file-received",
                data,
                serde_json::json!({ "id": id, "name": name }).to_string(),
            ),
            FileEvent::Sent { id } => ("file-sent", Vec::new(), serde_json::json!({ "id": id }).to_string()),
            FileEvent::Failed { id, message } => (
                "file-error",
                Vec::new(),
                serde_json::json!({ "id": id, "message": message }).to_string(),
            ),
        };
        Self { kind, bytes, text }
    }
}

#[wasm_bindgen]
impl Viewer {
    #[wasm_bindgen(constructor)]
    pub fn new(password: &str, auth_code: &str) -> Viewer {
        Viewer {
            session: ViewerSession::new(password, auth_code),
            events: VecDeque::new(),
        }
    }

    pub fn receive(&mut self, message: &[u8], now_ms: f64) {
        let events = self.session.receive(message, now_ms);
        self.events.extend(events);
    }

    pub fn closed(&mut self) {
        let events = self.session.closed();
        self.events.extend(events);
    }

    pub fn tick(&mut self, now_ms: f64) {
        let events = self.session.tick(now_ms);
        self.events.extend(events);
    }

    #[wasm_bindgen(js_name = nextEvent)]
    pub fn next_event(&mut self) -> Option<ViewerEvent> {
        self.events.pop_front().map(Into::into)
    }

    pub fn input(&mut self, event: &[u8]) -> Option<Vec<u8>> {
        self.session.input(event)
    }

    /// Seal local clipboard text for the host. `None` if too large or not yet live.
    pub fn clipboard(&mut self, text: &str) -> Option<Vec<u8>> {
        self.session.clipboard(text)
    }

    #[wasm_bindgen(js_name = clipboardImage)]
    pub fn clipboard_image(&mut self, width: u32, height: u32, png: Vec<u8>) -> Option<Vec<u8>> {
        self.session.clipboard_image(width, height, png)
    }

    /// Offer a local file under the page-chosen `id` (unique, below 2^53).
    #[wasm_bindgen(js_name = sendFile)]
    pub fn send_file(&mut self, id: f64, name: &str, data: Vec<u8>) -> Option<Vec<u8>> {
        self.session.send_file(id as u64, name, data)
    }

    /// Accept an incoming file offer.
    #[wasm_bindgen(js_name = acceptFile)]
    pub fn accept_file(&mut self, id: f64) -> Option<Vec<u8>> {
        self.session.accept_file(id as u64)
    }

    /// Reject an incoming file offer.
    #[wasm_bindgen(js_name = rejectFile)]
    pub fn reject_file(&mut self, id: f64) -> Option<Vec<u8>> {
        self.session.reject_file(id as u64)
    }

    #[wasm_bindgen(js_name = requestKeyframe)]
    pub fn request_keyframe(&mut self) -> Option<Vec<u8>> {
        self.session.request_keyframe()
    }

    #[wasm_bindgen(js_name = selectMonitor)]
    pub fn select_monitor(&mut self, id: u32) -> Option<Vec<u8>> {
        self.session.select_monitor(id)
    }

    pub fn bye(&mut self) -> Option<Vec<u8>> {
        self.session.bye()
    }
}

#[wasm_bindgen(js_name = connectFrame)]
pub fn connect_frame(target: &str, viewer_id: &str) -> Result<Vec<u8>, JsError> {
    crate::connect_frame(target, viewer_id).map_err(|error| JsError::new(&error))
}

/// The host's reject code inside a session-rejection error, e.g. "2fa_required".
/// `undefined` when the error is not a host rejection.
#[wasm_bindgen(js_name = rejectionCode)]
pub fn rejection_code(error: &str) -> Option<String> {
    reconnect::rejection_code(error).map(str::to_owned)
}

/// What to do after a failed attempt. `retry` false means stop and show `message`.
#[wasm_bindgen]
pub struct Step {
    pub retry: bool,
    #[wasm_bindgen(js_name = delayMs)]
    pub delay_ms: f64,
    message: String,
}

#[wasm_bindgen]
impl Step {
    #[wasm_bindgen(getter)]
    pub fn message(&self) -> String {
        self.message.clone()
    }
}

#[wasm_bindgen(js_name = afterFailure)]
pub fn after_failure(message: &str, session_started: bool, attempt: u32, outage_ms: f64) -> Step {
    let outage = Duration::from_millis(outage_ms.max(0.0) as u64);
    match reconnect::after_failure(message, session_started, attempt, outage) {
        reconnect::NextStep::Retry { delay, notice } => Step {
            retry: true,
            delay_ms: delay.as_millis() as f64,
            message: notice,
        },
        reconnect::NextStep::Stop { message } => Step {
            retry: false,
            delay_ms: 0.0,
            message,
        },
    }
}
