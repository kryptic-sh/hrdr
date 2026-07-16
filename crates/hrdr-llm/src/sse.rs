//! Incremental SSE (Server-Sent Events) decoder.
//!
//! Feed raw byte chunks with [`SseDecoder::push`]; drain complete events with
//! [`SseDecoder::drain`].  The decoder is spec-correct (HTML Living Standard §9.2):
//!
//! - Events are terminated by a blank line (`\n\n` or `\r\n\r\n`).
//! - Multiple `data:` lines in one event are folded with `\n`.
//! - The `event:` field sets the event type.
//! - One leading ASCII space after the colon is stripped (per spec §9.2.6).
//! - `id:`, `retry:`, and comment lines (`:`) are silently ignored.
//!
//! **Chunk-split safety.** Because `0x0A` (LINE FEED) never appears inside a
//! multi-byte UTF-8 sequence, buffering raw bytes per-line is safe: a codepoint
//! split across two `push` calls is buffered whole inside `line_buf` and decoded
//! only when the terminating `\n` arrives.  No bytes are lost or corrupted.

/// Hard cap on how large `line_buf` (one partial line) or `cur_data` (one
/// event's folded `data:` value) may grow before the decoder stops
/// accumulating further bytes for the offending line/event. Bounds memory
/// against a broken or hostile server that never sends a terminating newline
/// (or blank line) — without this, [`SseDecoder::push`] would grow these
/// buffers without limit for as long as bytes keep arriving. Tens of MB is
/// far beyond any legitimate SSE line or event payload (chat deltas run
/// bytes to low KB).
const MAX_BUFFER_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

/// One complete SSE event, yielded by [`SseDecoder`] after its blank-line
/// terminator is received.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field value, if present.
    pub event: Option<String>,
    /// The `data:` value — multiple `data:` lines are folded with `\n` per
    /// the SSE spec (§9.2.6: append field value then U+000A to the data buffer).
    pub data: String,
}

/// Incremental SSE decoder.
///
/// Feed raw byte chunks with [`Self::push`]; drain complete events with
/// [`Self::drain`].
///
/// # Example
///
/// ```
/// # use hrdr_llm::SseDecoder;
/// let mut dec = SseDecoder::new();
/// dec.push(b"data: hello\n\n");
/// let events = dec.drain();
/// assert_eq!(events[0].data, "hello");
/// ```
pub struct SseDecoder {
    /// Raw bytes accumulated for the current partial line (not yet `\n`-terminated).
    line_buf: Vec<u8>,
    /// `event:` field for the current event block.
    cur_event: Option<String>,
    /// Accumulated `data:` value; multiple `data:` lines joined by `\n`.
    cur_data: String,
    /// Whether any `data:` line has been seen in the current event block.
    cur_data_started: bool,
    /// Complete events ready for the next [`Self::drain`] call.
    ready: Vec<SseEvent>,
    /// Set once [`MAX_BUFFER_BYTES`] has been hit and excess bytes were
    /// discarded rather than buffered. See [`Self::overflowed`].
    overflowed: bool,
}

impl SseDecoder {
    /// Create a new, empty decoder.
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            cur_event: None,
            cur_data: String::new(),
            cur_data_started: false,
            ready: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a raw byte chunk.  Call [`Self::drain`] after each push to retrieve
    /// any complete events that these bytes completed.
    ///
    /// Bytes belonging to a single unterminated line beyond [`MAX_BUFFER_BYTES`]
    /// are discarded rather than buffered — a broken or hostile server that
    /// never sends a newline must not grow memory without bound. See
    /// [`Self::overflowed`].
    pub fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                self.flush_line();
            } else if self.line_buf.len() < MAX_BUFFER_BYTES {
                self.line_buf.push(b);
            } else {
                self.overflowed = true;
            }
        }
    }

    /// Whether the decoder has ever hit [`MAX_BUFFER_BYTES`] and discarded
    /// excess bytes for a line or an event's folded `data:` value. The decoder
    /// keeps running (memory stays bounded either way) — this is a signal for
    /// callers that want to treat a stream this misbehaved as an error rather
    /// than silently accept truncated data.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Flush the current line buffer: decode, classify the field, and on a
    /// blank line emit any complete pending event to the ready queue.
    fn flush_line(&mut self) {
        // 0x0A (LF) never appears inside a multi-byte UTF-8 sequence, so every
        // `line_buf` produced by splitting on `\n` is a complete sequence of
        // UTF-8 codepoints — UTF-8 decoding is safe even if the *original*
        // chunk boundary split a codepoint mid-byte.
        let raw = std::mem::take(&mut self.line_buf);
        let decoded = String::from_utf8_lossy(&raw);
        // Strip a trailing CR so CRLF (\r\n) line endings work transparently.
        let line = decoded.strip_suffix('\r').unwrap_or(&decoded);

        if line.is_empty() {
            // Blank line = event terminator.  Emit only when at least one
            // `data:` line was seen (suppress comment-only or `event:`-only
            // blocks, and the initial blank line some transports send).
            if self.cur_data_started {
                self.ready.push(SseEvent {
                    event: self.cur_event.take(),
                    data: std::mem::take(&mut self.cur_data),
                });
                self.cur_data_started = false;
            } else {
                self.cur_event = None;
            }
            return;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            // Strip exactly one leading space per spec §9.2.6.
            let value = rest.strip_prefix(' ').unwrap_or(rest);
            // Cap the folded `data:` value at MAX_BUFFER_BYTES: an event whose
            // blank-line terminator never arrives could otherwise accumulate
            // `data:` lines forever. Account for the folding '\n' up front so
            // the total never exceeds the cap even after appending it.
            let sep_len = if self.cur_data_started { 1 } else { 0 };
            let remaining = MAX_BUFFER_BYTES.saturating_sub(self.cur_data.len() + sep_len);
            if remaining == 0 {
                self.overflowed = true;
            } else {
                if self.cur_data_started {
                    self.cur_data.push('\n');
                }
                if value.len() <= remaining {
                    self.cur_data.push_str(value);
                } else {
                    // Truncate at a char boundary so we never split a UTF-8
                    // sequence and produce an invalid `String`.
                    let mut cut = remaining;
                    while cut > 0 && !value.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    self.cur_data.push_str(&value[..cut]);
                    self.overflowed = true;
                }
            }
            self.cur_data_started = true;
        } else if let Some(rest) = line.strip_prefix("event:") {
            let value = rest.strip_prefix(' ').unwrap_or(rest);
            self.cur_event = Some(value.to_string());
        }
        // `id:`, `retry:`, and `:` (comment) lines are intentionally ignored.
    }

    /// Drain and return all complete events accumulated since the last call.
    /// Returns an empty `Vec` when no events are ready.
    pub fn drain(&mut self) -> Vec<SseEvent> {
        std::mem::take(&mut self.ready)
    }

    /// Flush at end-of-stream: emit any event whose `data:` was received but
    /// whose blank-line terminator never arrived (the byte stream closed right
    /// after the last line). This restores the leniency of a line-based parser
    /// — many OpenAI-compatible servers end with `data: [DONE]\n` (or even no
    /// trailing newline) rather than a spec `\n\n`, and the final event must not
    /// be silently dropped (which would look like a truncated stream). Returns
    /// the trailing events plus anything still queued.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        // A trailing line with no terminating `\n` is still a complete line at EOF.
        if !self.line_buf.is_empty() {
            self.flush_line();
        }
        // A `data:` block with no blank-line terminator is still a complete event
        // once the stream ends.
        if self.cur_data_started {
            self.ready.push(SseEvent {
                event: self.cur_event.take(),
                data: std::mem::take(&mut self.cur_data),
            });
            self.cur_data_started = false;
        }
        std::mem::take(&mut self.ready)
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push all `chunks` through a fresh decoder and drain the results.
    fn feed(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut dec = SseDecoder::new();
        for chunk in chunks {
            dec.push(chunk);
        }
        dec.drain()
    }

    // ── blank-line event termination ──────────────────────────────────────────

    #[test]
    fn simple_event_blank_line_terminated() {
        let events = feed(&[b"data: hello\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert!(events[0].event.is_none());
    }

    // ── event: field ──────────────────────────────────────────────────────────

    #[test]
    fn event_field_is_parsed() {
        let events = feed(&[b"event: ping\ndata: world\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "world");
    }

    // ── leading-space trimming ────────────────────────────────────────────────

    #[test]
    fn leading_space_after_colon_stripped_spec_correct() {
        // One leading space is stripped (spec §9.2.6).
        let events = feed(&[b"data: hello\n\n"]);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn no_space_after_colon_value_used_verbatim() {
        let events = feed(&[b"data:hello\n\n"]);
        assert_eq!(events[0].data, "hello");
    }

    // ── multi-line data folding ───────────────────────────────────────────────

    #[test]
    fn multi_line_data_folded_with_newline() {
        let events = feed(&[b"data: line1\ndata: line2\n\n"]);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn multi_line_data_forms_valid_json_when_split_across_lines() {
        // Mirrors the MCP multi-line data payload test: split a JSON object
        // across two `data:` lines and verify it folds into parseable JSON.
        let input = b"data: {\"a\":1,\ndata: \"b\":2}\n\n";
        let events = feed(&[input]);
        assert_eq!(events[0].data, "{\"a\":1,\n\"b\":2}");
        // serde_json accepts leading/internal whitespace.
        let v: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    // ── CRLF ─────────────────────────────────────────────────────────────────

    #[test]
    fn crlf_line_endings_accepted() {
        let events = feed(&[b"data: hi\r\n\r\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn mixed_crlf_and_lf_accepted() {
        let events = feed(&[b"event: e\r\ndata: d\r\n\r\n"]);
        assert_eq!(events[0].event.as_deref(), Some("e"));
        assert_eq!(events[0].data, "d");
    }

    // ── chunk-boundary splits ─────────────────────────────────────────────────

    #[test]
    fn chunk_split_mid_line() {
        // "data: hello\n\n" split after "data:"
        let events = feed(&[b"data:", b" hello\n\n"]);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn chunk_split_mid_data_prefix() {
        // Split inside the "data" keyword itself.
        let events = feed(&[b"dat", b"a: world\n\n"]);
        assert_eq!(events[0].data, "world");
    }

    #[test]
    fn chunk_split_across_event_separator() {
        // The \n\n terminator split across two pushes.
        let events_a = {
            let mut dec = SseDecoder::new();
            dec.push(b"data: x\n");
            dec.push(b"\n");
            dec.drain()
        };
        assert_eq!(events_a.len(), 1);
        assert_eq!(events_a[0].data, "x");
    }

    #[test]
    fn chunk_split_mid_utf8_codepoint() {
        // The Euro sign €  is 3 bytes: 0xE2 0x82 0xAC.
        // Split after the first byte — the decoder must not lose or corrupt it.
        let eur = "€";
        let bytes = eur.as_bytes(); // [0xE2, 0x82, 0xAC]
        assert_eq!(bytes.len(), 3);

        let part1: Vec<u8> = [b"data: ".as_ref(), &bytes[..1]].concat();
        let part2: Vec<u8> = [&bytes[1..], b"\n\n"].concat();
        let events = feed(&[&part1, &part2]);
        assert_eq!(
            events[0].data, "€",
            "mid-codepoint split must not corrupt the character"
        );
    }

    // ── [DONE] passthrough ────────────────────────────────────────────────────

    #[test]
    fn done_sentinel_is_plain_data_payload() {
        // [DONE] is just a normal `data:` value from the decoder's perspective;
        // the caller (OpenAI client) is responsible for treating it as a
        // stream-end sentinel.
        let events = feed(&[b"data: [DONE]\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
    }

    // ── multiple events ───────────────────────────────────────────────────────

    #[test]
    fn multiple_events_in_one_push() {
        let input = b"data: first\n\ndata: second\n\n";
        let events = feed(&[input]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    // ── drain behaviour ───────────────────────────────────────────────────────

    #[test]
    fn drain_clears_ready_queue() {
        let mut dec = SseDecoder::new();
        dec.push(b"data: x\n\n");
        assert_eq!(dec.drain().len(), 1);
        assert_eq!(dec.drain().len(), 0, "second drain must be empty");
    }

    // ── empty / degenerate input ──────────────────────────────────────────────

    #[test]
    fn blank_line_without_data_does_not_emit_event() {
        // A blank line with no preceding data must not yield an event.
        let events = feed(&[b"\n\n"]);
        assert!(events.is_empty());
    }

    #[test]
    fn event_only_without_data_does_not_emit_event() {
        // A block with only `event:` and no `data:` must be silently dropped.
        let events = feed(&[b"event: ping\n\n"]);
        assert!(events.is_empty());
    }

    #[test]
    fn empty_push_does_nothing() {
        let events = feed(&[b""]);
        assert!(events.is_empty());
    }

    // ── finish() flushes an unterminated trailing event ───────────────────────

    #[test]
    fn finish_flushes_data_without_blank_line_terminator() {
        // A stream that ends `data: [DONE]\n` (single newline, no blank line) —
        // common with llama.cpp/vLLM/infr — must still yield the final event.
        let mut dec = SseDecoder::new();
        dec.push(b"data: [DONE]\n");
        assert!(dec.drain().is_empty(), "no blank line yet, nothing ready");
        let ev = dec.finish();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "[DONE]");
    }

    #[test]
    fn finish_flushes_line_without_any_newline() {
        // Stream closes with no trailing newline at all.
        let mut dec = SseDecoder::new();
        dec.push(b"data: {\"x\":1}");
        let ev = dec.finish();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "{\"x\":1}");
    }

    #[test]
    fn finish_after_clean_terminator_yields_nothing() {
        // A properly `\n\n`-terminated stream leaves nothing for finish().
        let mut dec = SseDecoder::new();
        dec.push(b"data: x\n\n");
        assert_eq!(dec.drain().len(), 1);
        assert!(dec.finish().is_empty());
    }

    // ── unbounded-buffer DoS guard ────────────────────────────────────────────

    #[test]
    fn line_buffer_is_capped_and_flagged_overflowed() {
        // A hostile/broken server that never sends a newline at all: line_buf
        // must not grow past MAX_BUFFER_BYTES no matter how many bytes arrive.
        let mut dec = SseDecoder::new();
        let chunk = vec![b'x'; MAX_BUFFER_BYTES + 1024];
        dec.push(&chunk);
        assert!(dec.overflowed(), "cap should have been hit");
        assert_eq!(
            dec.line_buf.len(),
            MAX_BUFFER_BYTES,
            "buffer stays capped, not grown further"
        );

        // Feeding still more bytes (still no newline) must not grow it either.
        dec.push(&[b'y'; 1024]);
        assert_eq!(dec.line_buf.len(), MAX_BUFFER_BYTES);
    }

    #[test]
    fn cur_data_is_capped_without_corrupting_utf8() {
        // An event whose blank-line terminator never arrives, fed `data:`
        // lines that together would otherwise grow cur_data without bound.
        let mut dec = SseDecoder::new();
        let big = "a".repeat(MAX_BUFFER_BYTES - 10);
        dec.push(format!("data: {big}\n").as_bytes());
        assert!(!dec.overflowed(), "not over the cap yet");

        // Push another data line that would push cur_data past the cap.
        dec.push(b"data: this-line-does-not-fit-in-the-remaining-room\n");
        assert!(dec.overflowed(), "cap should now be flagged");
        assert!(dec.cur_data.len() <= MAX_BUFFER_BYTES);

        // Terminate the event and confirm the folded value is valid UTF-8 and
        // bounded — no panic, no corrupted string.
        dec.push(b"\n");
        let events = dec.drain();
        assert_eq!(events.len(), 1);
        assert!(events[0].data.len() <= MAX_BUFFER_BYTES);
    }
}
