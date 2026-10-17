//! The hjkl `Host` implementation hrdr's editor runs against.
//!
//! Mirrors the well-trodden sqeel adapter: the host **owns the viewport** (the
//! renderer publishes dimensions, the engine writes scroll offsets). Clipboard
//! writes queue to an outbox that `VimEngine::feed_key` flushes after each key;
//! reads pull the OS clipboard directly (the same `hjkl_clipboard::get` the
//! TUI's `/paste` uses — a paste is user-initiated and rare, so it needn't be
//! cached). Both are synchronous OS calls on the caller's thread.

use std::time::{Duration, Instant};

use hjkl_clipboard::{Clipboard, MimeType, Selection};
use hjkl_engine::types::Viewport;
use hjkl_engine::{CursorShape, Host};

pub struct HrdrHost {
    clipboard: Option<Clipboard>,
    clipboard_outbox: Vec<String>,
    /// A clipboard failure the user has not been told about yet.
    clipboard_notice: Option<&'static str>,
    /// Whether the last write failed, so a run of failures reports only once.
    clipboard_failing: bool,
    started: Instant,
    viewport: Viewport,
}

impl HrdrHost {
    pub fn new() -> Self {
        Self {
            clipboard: Clipboard::new().ok(),
            clipboard_outbox: Vec::new(),
            clipboard_notice: None,
            clipboard_failing: false,
            started: Instant::now(),
            // Long lines wrap at word edges, as the plain engine's do, rather
            // than scrolling the prompt sideways.
            viewport: Viewport {
                width: 80,
                height: 24,
                wrap: hjkl_buffer::Wrap::Word,
                ..Viewport::default()
            },
        }
    }

    /// Flush queued clipboard writes to the OS (call once per tick). A failed
    /// write — or no clipboard at all — is kept for [`Self::take_clipboard_notice`],
    /// once per run of failures: a box without a clipboard would otherwise warn
    /// on every `x`.
    pub fn flush_clipboard(&mut self) {
        for text in std::mem::take(&mut self.clipboard_outbox) {
            let failure = match &self.clipboard {
                None => Some("clipboard unavailable"),
                Some(cb) => cb
                    .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                    .err()
                    .map(|_| "clipboard write failed"),
            };
            match failure {
                Some(why) if !self.clipboard_failing => {
                    self.clipboard_failing = true;
                    self.clipboard_notice = Some(why);
                }
                Some(_) => {}
                None => self.clipboard_failing = false,
            }
        }
    }

    /// The clipboard failure to tell the user about, if one is pending.
    pub fn take_clipboard_notice(&mut self) -> Option<&'static str> {
        self.clipboard_notice.take()
    }
}

/// Clipboard bytes as buffer text: UTF-8, with every `\r` dropped. Windows
/// clipboard text uses CRLF, and hjkl keeps a `\r` in the buffer, so `"+p`
/// would put stray carriage returns in the message — the same reason the
/// editor's `paste` strips them.
fn clipboard_text(bytes: Vec<u8>) -> Option<String> {
    String::from_utf8(bytes)
        .ok()
        .map(|text| text.replace('\r', ""))
}

impl Default for HrdrHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for HrdrHost {
    type Intent = ();

    fn write_clipboard(&mut self, text: String) {
        self.clipboard_outbox.push(text);
    }

    fn read_clipboard(&mut self) -> Option<String> {
        let bytes = self
            .clipboard
            .as_ref()?
            .get(Selection::Clipboard, MimeType::Text)
            .ok()?;
        clipboard_text(bytes)
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn should_cancel(&self) -> bool {
        false
    }

    fn prompt_search(&mut self) -> Option<String> {
        None
    }

    fn emit_cursor_shape(&mut self, _shape: CursorShape) {}

    fn emit_intent(&mut self, _intent: Self::Intent) {}

    fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRLF clipboard text reaches the buffer as LF; bytes that are not UTF-8
    /// read as nothing.
    #[test]
    fn clipboard_text_drops_carriage_returns() {
        assert_eq!(
            clipboard_text(b"one\r\ntwo\r\n".to_vec()).as_deref(),
            Some("one\ntwo\n")
        );
        assert_eq!(clipboard_text(vec![0xff]), None);
    }

    /// Without a clipboard, a yank reports once — not once per write, and not
    /// again until a write has succeeded.
    #[test]
    fn a_missing_clipboard_reports_once_per_run_of_failures() {
        let mut host = HrdrHost {
            clipboard: None,
            ..HrdrHost::new()
        };
        host.write_clipboard("a".into());
        host.write_clipboard("b".into());
        host.flush_clipboard();
        assert_eq!(host.take_clipboard_notice(), Some("clipboard unavailable"));
        assert_eq!(host.take_clipboard_notice(), None);
        host.write_clipboard("c".into());
        host.flush_clipboard();
        assert_eq!(host.take_clipboard_notice(), None);
    }
}
