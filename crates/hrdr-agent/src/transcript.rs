//! The transcript data model shared across hrdr: the [`Entry`] struct (one
//! rendered item in the conversation, with the time it arrived) plus the
//! representation-independent queries over a slice of entries — search, message
//! counting/indexing, and text export. How an `Entry` is painted is the
//! frontend's business; what counts as a "message", how `/find` matches, and the
//! export formats are shared here so every frontend stays consistent.
//!
//! `Entry` is also the on-disk form: a session file stores its transcript as
//! exactly these entries, so a resume restores what was on screen without a
//! lossy rebuild from the chat messages.

use crate::AgentEvent;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// One rendered item in the transcript, stamped with the local time it was
/// added. Serializes as a flat object: `{"kind": "user", "data": "hi",
/// "time": 1700000000}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub kind: EntryKind,
    /// When this entry was added, stored as unix seconds.
    #[serde(with = "unix_time")]
    pub time: DateTime<Local>,
    /// Precomputed hash of the content fields that affect rendering (excludes
    /// timestamps, `took_ms`, and the Tool `expanded` flag / `expand_all`).
    /// Computed on construction and refreshed on mutation. Never serialized.
    #[serde(skip, default)]
    pub content_hash: u64,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.time == other.time
    }
}

/// What an [`Entry`] holds. Everything here round-trips through the session
/// file except a tool block's `expanded` flag, which is view state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EntryKind {
    /// The session banner: logo animation on the left, session details on the
    /// right. Carries no data — the details are read from the live session
    /// state at render time, so a resumed header shows the *current* model and
    /// provider rather than whatever was in use when it was written.
    Header,
    User(String),
    Assistant(String),
    /// A block of the model's thinking. `took_ms` is set when the block ends,
    /// and drives the `Thought: 1.2s` label; while `None` the block is still
    /// streaming and the frontend shows a spinner instead. The label is never
    /// part of `text` — it's chrome the renderer adds.
    Reasoning {
        text: String,
        #[serde(default)]
        took_ms: Option<u64>,
    },
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
        /// Show the full result instead of a truncated preview (`/expand`).
        /// View state: never persisted, so a restored block starts collapsed.
        #[serde(skip)]
        expanded: bool,
    },
    System(String),
    /// Session-lifecycle chrome: the welcome banner, "resumed session …",
    /// "session saved as …", "config reloaded …". Rendered like [`Self::System`]
    /// but **never persisted** — every launch and every resume regenerates its
    /// own, so saving them would accrete a fresh copy per resume.
    Notice(String),
    /// Final per-turn stats line, appended below the last output.
    Stats(String),
    /// A unified diff (e.g. `/diff`), rendered with diff coloring.
    Diff(String),
}

impl Entry {
    /// An entry stamped with the current local time.
    pub fn now(kind: EntryKind) -> Self {
        let content_hash = Self::kind_hash(&kind);
        Self {
            kind,
            time: Local::now(),
            content_hash,
        }
    }

    /// An entry stamped with an explicit time (restoring, or in tests).
    pub fn at(kind: EntryKind, time: DateTime<Local>) -> Self {
        let content_hash = Self::kind_hash(&kind);
        Self {
            kind,
            time,
            content_hash,
        }
    }

    /// The banner that opens a new session.
    pub fn header() -> Self {
        Self::now(EntryKind::Header)
    }

    /// Hash of the content fields that affect rendering. Excludes timestamps,
    /// `took_ms`, and the Tool `expanded` flag / `expand_all` — the latter two
    /// are combined at lookup time in the frontend.
    fn kind_hash(kind: &EntryKind) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match kind {
            EntryKind::Header => {}
            EntryKind::Reasoning { text, .. } => text.hash(&mut h),
            EntryKind::User(t)
            | EntryKind::Assistant(t)
            | EntryKind::System(t)
            | EntryKind::Notice(t)
            | EntryKind::Stats(t)
            | EntryKind::Diff(t) => t.hash(&mut h),
            EntryKind::Tool {
                name,
                args,
                result,
                ok,
                done,
                ..
            } => {
                name.hash(&mut h);
                args.hash(&mut h);
                result.hash(&mut h);
                ok.hash(&mut h);
                done.hash(&mut h);
                // expanded and expand_all are handled at cache-key lookup time
            }
        }
        h.finish()
    }

    /// Recompute `content_hash` after an in-place mutation (text push, tool
    /// result/ok/done change). Called by the event-applier in `pane.rs`.
    pub fn refresh_hash(&mut self) {
        self.content_hash = Self::kind_hash(&self.kind);
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::now(EntryKind::User(text.into()))
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::now(EntryKind::Assistant(text.into()))
    }
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::now(EntryKind::Reasoning {
            text: text.into(),
            took_ms: None,
        })
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self::now(EntryKind::System(text.into()))
    }
    /// Ephemeral session chrome — see [`EntryKind::Notice`].
    pub fn notice(text: impl Into<String>) -> Self {
        Self::now(EntryKind::Notice(text.into()))
    }
    pub fn stats(text: impl Into<String>) -> Self {
        Self::now(EntryKind::Stats(text.into()))
    }
    pub fn diff(text: impl Into<String>) -> Self {
        Self::now(EntryKind::Diff(text.into()))
    }

    /// A tool call that has not finished yet.
    pub fn tool_running(
        id: impl Into<String>,
        name: impl Into<String>,
        args: impl Into<String>,
    ) -> Self {
        Self::now(EntryKind::Tool {
            id: id.into(),
            name: name.into(),
            args: args.into(),
            result: String::new(),
            ok: false,
            done: false,
            expanded: false,
        })
    }
}

/// A tool call restored from disk can never finish, so `done: false` would spin
/// its spinner forever — settle it as failed instead.
pub fn settle_restored_tools(entries: &mut [Entry]) {
    for e in entries {
        if let EntryKind::Tool { ok, done, .. } = &mut e.kind
            && !*done
        {
            *done = true;
            *ok = false;
        }
    }
}

/// A unix-second timestamp as a local time, falling back to `fallback` for a
/// value chrono can't represent — a corrupt file shouldn't wrap into a
/// plausible pre-epoch date (which `as i64` would do), nor panic.
pub fn time_from_unix(secs: i64, fallback: DateTime<Local>) -> DateTime<Local> {
    DateTime::from_timestamp(secs, 0)
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or(fallback)
}

/// A `SystemTime` as a local timestamp. The agent domain has no chrono, so a
/// turn's wall-clock start ([`crate::TurnStats::started_at`]) crosses over as
/// a `SystemTime` and is rendered here.
pub fn time_from_system(t: std::time::SystemTime) -> DateTime<Local> {
    DateTime::<Local>::from(t)
}

/// `DateTime<Local>` as unix seconds, for [`Entry`]'s `time` field.
mod unix_time {
    use chrono::{DateTime, Local, TimeZone};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &DateTime<Local>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(t.timestamp())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Local>, D::Error> {
        let secs = i64::deserialize(d)?;
        let epoch = Local.timestamp_opt(0, 0).unwrap();
        Ok(super::time_from_unix(secs, epoch))
    }
}

/// Extract the `command` field from a JSON tool-args string, if any.
/// Returns `None` for non-shell tools or malformed args.
pub fn extract_shell_command(name: &str, args: &str) -> Option<String> {
    if name != "shell" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(String::from))
}

/// How a tool call's detail area should be painted. Frontends map each variant
/// onto their own renderer; the classification (which tool shows what) lives
/// here so every frontend agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolBody {
    /// Shell call: the command, rendered as its own `$ …` line above the output.
    Shell {
        command: String,
    },
    /// `write`: the file contents from the args, syntax-highlighted as `lang`
    /// (the path's extension, which syntect resolves as a token).
    Code {
        lang: String,
        content: String,
    },
    /// `edit`: the result is a unified diff — color it as one.
    Diff,
    /// `read`: the result's *tail* is the interesting part (the file content,
    /// not the preamble).
    Read,
    /// Any other call (`task`, `todo`, an MCP tool): its arguments as
    /// `key: value` rows below the tool's name, rather than raw JSON beside it.
    /// Values are single-line; the caller decides how far to truncate them.
    Details(Vec<(String, String)>),
    Text,
}

/// The headline (shown after the tool name) plus the detail body for one tool
/// call, derived from its name and raw JSON args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDisplay {
    /// Short summary shown on the header line: a path, a pattern, or an args
    /// preview. Empty when the detail body already says it (shell).
    pub headline: String,
    pub body: ToolBody,
}

/// Pull a string field out of a JSON tool-args object.
fn arg_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(String::from)
}

/// Classify a tool call for display: what goes on the header line, and how the
/// detail area is rendered. Falls back to a truncated args preview + plain text
/// for tools with no special treatment (including MCP tools).
pub fn tool_display(name: &str, args: &str) -> ToolDisplay {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let plain = |headline: String| ToolDisplay {
        headline,
        body: ToolBody::Text,
    };
    match name {
        "shell" => ToolDisplay {
            headline: String::new(),
            body: ToolBody::Shell {
                command: arg_str(&v, "command").unwrap_or_default(),
            },
        },
        "write" => {
            let path = arg_str(&v, "path").unwrap_or_else(|| "?".into());
            let lang = std::path::Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            ToolDisplay {
                headline: path,
                body: ToolBody::Code {
                    lang,
                    content: arg_str(&v, "content").unwrap_or_default(),
                },
            }
        }
        "edit" => ToolDisplay {
            headline: arg_str(&v, "path").unwrap_or_else(|| "?".into()),
            body: ToolBody::Diff,
        },
        "read" => ToolDisplay {
            headline: read_args_summary(&v),
            body: ToolBody::Read,
        },
        "grep" | "find" => {
            let mut s = arg_str(&v, "pattern").unwrap_or_default();
            if let Some(p) = arg_str(&v, "path") {
                s.push_str(&format!("  in {p}"));
            }
            if let Some(g) = arg_str(&v, "glob") {
                s.push_str(&format!("  ({g})"));
            }
            plain(s)
        }
        "ls" | "tree" => plain(arg_str(&v, "path").unwrap_or_else(|| ".".into())),
        // The tool result already contains the normalized replacement list.
        // Rendering the input too duplicates every item and can disagree with
        // normalization performed by the tool.
        "todo" => plain(String::new()),
        // Git: show the subcommand and its args inline, like `git status --short`,
        // rather than rendering each field as a separate details row.
        "git" => {
            let sub = arg_str(&v, "subcommand").unwrap_or_default();
            let args_str: String = v
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty())
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            plain(format!("{sub}{args_str}"))
        }
        // `task` and every MCP tool: their arguments, one per row.
        _ => ToolDisplay {
            headline: String::new(),
            body: ToolBody::Details(arg_details(&v)),
        },
    }
}

/// A JSON value as one display line: strings bare, everything else compact JSON.
/// Newlines and tabs collapse to spaces so a row stays a row.
fn detail_value(v: &serde_json::Value) -> String {
    let raw = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let flat: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A tool call's arguments as `(key, value)` rows, sorted by key — `serde_json`
/// deserializes an object into a `BTreeMap` unless `preserve_order` is on, so
/// the model's key order isn't available here. A non-object argument (rare, but
/// a tool may take a bare string or array) becomes a single unlabelled row.
fn arg_details(v: &serde_json::Value) -> Vec<(String, String)> {
    match v {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, val)| (k.clone(), detail_value(val)))
            .collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![(String::new(), detail_value(other))],
    }
}

/// Compact summary of `read` args: `path  (offset: N, limit: M)`.
fn read_args_summary(v: &serde_json::Value) -> String {
    let mut s = v
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("?")
        .to_string();
    let mut parts = Vec::new();
    if let Some(o) = v.get("offset").and_then(|o| o.as_u64()).filter(|&o| o > 1) {
        parts.push(format!("offset: {o}"));
    }
    if let Some(l) = v.get("limit").and_then(|l| l.as_u64()) {
        parts.push(format!("limit: {l}"));
    }
    if !parts.is_empty() {
        s.push_str(&format!("  ({})", parts.join(", ")));
    }
    s
}

impl Entry {
    /// The displayable text of a user/assistant message, if this entry is one.
    /// These are the only entries that count as numbered "messages" for `/find`,
    /// `/goto`, `/copy msg N`, and export.
    pub fn message_text(&self) -> Option<&str> {
        match &self.kind {
            // A text-less assistant turn (tool calls only) still counts: it has
            // no block of its own, but its `#N assistant` label rides on the
            // previous block as a `/goto` jump point.
            EntryKind::User(s) | EntryKind::Assistant(s) => Some(s),
            _ => None,
        }
    }
}

/// 1-based message numbers whose user/assistant text contains `query`
/// (case-insensitive substring). Message numbers count only user/assistant
/// entries, matching the numbering the frontends display.
pub fn find_hits(entries: &[Entry], query: &str) -> Vec<usize> {
    let needle = query.to_ascii_lowercase();
    let mut num = 0;
    let mut hits = Vec::new();
    for e in entries {
        if let Some(s) = e.message_text() {
            num += 1;
            if s.to_ascii_lowercase().contains(&needle) {
                hits.push(num);
            }
        }
    }
    hits
}

/// Number of user/assistant messages in the transcript.
pub fn message_count(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter(|e| e.message_text().is_some())
        .count()
}

/// The text of the Nth (1-based) user/assistant message, if any.
pub fn nth_message_text(entries: &[Entry], n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    entries
        .iter()
        .filter_map(Entry::message_text)
        .nth(n - 1)
        .map(str::to_string)
}

/// The number of the first user/assistant message stamped at/after `cutoff`.
/// `times` is parallel to `entries` (index i is entry i's local timestamp).
pub fn first_message_since(entries: &[Entry], cutoff: DateTime<Local>) -> Option<usize> {
    let mut num = 0;
    for e in entries {
        if e.message_text().is_some() {
            num += 1;
            if e.time >= cutoff {
                return Some(num);
            }
        }
    }
    None
}

/// The transcript as Markdown-ish text (user/assistant/system/diff/tool lines;
/// reasoning and stats are omitted). Used by `/copy all` and `/export`.
pub fn transcript_to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match &e.kind {
            EntryKind::User(s) => out.push_str(&format!("## User\n{s}\n\n")),
            EntryKind::Assistant(s) => out.push_str(&format!("## Assistant\n{s}\n\n")),
            EntryKind::System(s) => out.push_str(&format!("[{s}]\n\n")),
            EntryKind::Diff(s) => out.push_str(&format!("{s}\n\n")),
            EntryKind::Tool { name, .. } => out.push_str(&format!("[tool: {name}]\n\n")),
            EntryKind::Notice(s) => out.push_str(&format!("[{s}]\n\n")),
            EntryKind::Reasoning { .. } | EntryKind::Stats(_) | EntryKind::Header => {}
        }
    }
    out.trim_end().to_string()
}

/// Fold an agent event into a transcript. The shared reducer behind every pane —
/// the main agent's stream and a sub-agent's stream are assembled by the same
/// rules, so a sub-agent's view reads exactly like the main one.
///
/// Only transcript-visible events do anything; `Usage`, `History` and `TurnDone`
/// carry no transcript content and are ignored here (a frontend still handles
/// them for its own bookkeeping).
pub fn apply_event(transcript: &mut Vec<Entry>, ev: &AgentEvent) {
    // Close an open reasoning block as soon as anything else arrives, so its
    // duration label stops streaming.
    if !matches!(ev, AgentEvent::Reasoning(_)) {
        finish_reasoning(transcript);
    }
    match ev {
        AgentEvent::Text(t) => {
            let mut mutated = false;
            if let Some(last) = transcript.last_mut()
                && let EntryKind::Assistant(s) = &mut last.kind
            {
                s.push_str(t);
                last.refresh_hash();
                mutated = true;
            }
            if !mutated && !t.is_empty() {
                transcript.push(Entry::assistant(t.clone()));
            }
        }
        AgentEvent::Reasoning(t) => {
            let mut mutated = false;
            if let Some(last) = transcript.last_mut()
                && let EntryKind::Reasoning {
                    text,
                    took_ms: None,
                } = &mut last.kind
            {
                text.push_str(t);
                last.refresh_hash();
                mutated = true;
            }
            // Same `!is_empty` guard the `Text` arm above uses, and for a
            // sharper reason: an empty reasoning entry renders as nothing, but
            // it still lands at the tail of the transcript, and `Text` only
            // coalesces when the last entry is `Assistant`. So an empty delta
            // would silently split one streamed reply into a separate block per
            // chunk. Servers do emit these — a Qwen3-style backend keeps sending
            // `reasoning_content: ""` on every content chunk once it stops
            // thinking, where other providers omit the field entirely.
            if !mutated && !t.is_empty() {
                transcript.push(Entry::reasoning(t.clone()));
            }
        }
        AgentEvent::ToolStart { id, name, args } => {
            transcript.push(Entry::at(
                EntryKind::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    result: String::new(),
                    ok: true,
                    done: false,
                    expanded: false,
                },
                chrono::Local::now(),
            ));
        }
        AgentEvent::ToolOutput { id, chunk } => {
            if let Some(entry) = open_tool(transcript, id)
                && let EntryKind::Tool { result, .. } = &mut entry.kind
            {
                result.push_str(chunk);
                entry.refresh_hash();
            }
        }
        AgentEvent::ToolEnd {
            id,
            result,
            ok,
            name: _,
        } => {
            if let Some(entry) = open_tool(transcript, id)
                && let EntryKind::Tool {
                    result: r,
                    ok: o,
                    done,
                    ..
                } = &mut entry.kind
            {
                *r = result.clone();
                *o = *ok;
                *done = true;
                entry.refresh_hash();
            }
        }
        // An agent's notice (an error, an MCP warning, an exhausted step budget) is
        // something the agent said about the run, so it is a system line and it
        // persists — unlike frontend chrome (`Entry::notice`), which is stripped
        // from a saved session.
        AgentEvent::Notice(text) => transcript.push(Entry::system(text.clone())),
        // A steered message is a real user turn in this conversation.
        AgentEvent::Steered(sent) => transcript.push(Entry::user(sent.clone())),
        AgentEvent::Usage { .. }
        | AgentEvent::History(_)
        | AgentEvent::TurnDone
        | AgentEvent::TodoUpdated(_) => {}
    }
}

/// The still-open tool entry with `id`, searched from the end (a tool id is
/// unique within a turn, and the newest match is the live one).
fn open_tool<'a>(transcript: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    transcript.iter_mut().rev().find(|e| {
        matches!(&e.kind, EntryKind::Tool {
        id: tid,
        done: false,
        ..
    } if tid == id)
    })
}

/// Stamp a duration on a reasoning block that is still streaming. The frontend
/// owns the wall-clock, so this only marks it closed (`took_ms: Some(0)` would
/// lie); a frontend that tracks timing overwrites it.
fn finish_reasoning(transcript: &mut [Entry]) {
    if let Some(EntryKind::Reasoning {
        took_ms: took @ None,
        ..
    }) = transcript.last_mut().map(|e| &mut e.kind)
    {
        *took = Some(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            Entry::system("welcome"),
            Entry::user("Fix the parser bug"),
            Entry::reasoning("thinking…"),
            Entry::assistant("Done — it was an off-by-one."),
            Entry::user("thanks"),
        ]
    }

    #[test]
    fn message_count_and_nth_skip_non_messages() {
        let e = sample();
        assert_eq!(message_count(&e), 3); // 2 user + 1 assistant
        assert_eq!(
            nth_message_text(&e, 1).as_deref(),
            Some("Fix the parser bug")
        );
        assert_eq!(
            nth_message_text(&e, 2).as_deref(),
            Some("Done — it was an off-by-one.")
        );
        assert_eq!(nth_message_text(&e, 3).as_deref(), Some("thanks"));
        assert_eq!(nth_message_text(&e, 0), None);
        assert_eq!(nth_message_text(&e, 4), None);
    }

    #[test]
    fn find_hits_are_case_insensitive_message_numbers() {
        let e = sample();
        assert_eq!(find_hits(&e, "PARSER"), vec![1]);
        assert_eq!(find_hits(&e, "off-by-one"), vec![2]);
        // Reasoning/system are never matched even if they contain the needle.
        assert_eq!(find_hits(&e, "welcome"), Vec::<usize>::new());
        assert_eq!(find_hits(&e, "thinking"), Vec::<usize>::new());
    }

    #[test]
    fn to_text_omits_reasoning_and_stats() {
        let e = sample();
        let txt = transcript_to_text(&e);
        assert!(txt.contains("## User\nFix the parser bug"));
        assert!(txt.contains("## Assistant\nDone"));
        assert!(txt.contains("[welcome]"));
        assert!(!txt.contains("thinking")); // reasoning dropped
        assert!(!txt.ends_with('\n')); // trailing whitespace trimmed
    }
}

#[cfg(test)]
mod tool_display_tests {
    use super::*;

    #[test]
    fn shell_puts_the_command_in_the_body_not_the_headline() {
        let d = tool_display("shell", r#"{"command":"ls -la"}"#);
        assert!(d.headline.is_empty());
        assert_eq!(
            d.body,
            ToolBody::Shell {
                command: "ls -la".into()
            }
        );
    }

    #[test]
    fn write_shows_path_and_contents_with_the_extension_as_lang() {
        let d = tool_display("write", r#"{"path":"src/a.rs","content":"fn main() {}"}"#);
        assert_eq!(d.headline, "src/a.rs");
        assert_eq!(
            d.body,
            ToolBody::Code {
                lang: "rs".into(),
                content: "fn main() {}".into()
            }
        );
    }

    #[test]
    fn edit_shows_the_path_and_renders_a_diff() {
        let d = tool_display(
            "edit",
            r#"{"path":"x.rs","old_string":"a","new_string":"b"}"#,
        );
        assert_eq!(d.headline, "x.rs");
        assert_eq!(d.body, ToolBody::Diff);
    }

    #[test]
    fn read_summarizes_offset_and_limit() {
        let d = tool_display("read", r#"{"path":"x.rs","offset":10,"limit":5}"#);
        assert_eq!(d.headline, "x.rs  (offset: 10, limit: 5)");
        assert_eq!(d.body, ToolBody::Read);
        // offset 1 is the default — not worth the noise.
        let d = tool_display("read", r#"{"path":"x.rs","offset":1}"#);
        assert_eq!(d.headline, "x.rs");
    }

    #[test]
    fn grep_shows_pattern_scope_and_glob() {
        let d = tool_display("grep", r#"{"pattern":"fn ","path":"src","glob":"*.rs"}"#);
        assert_eq!(d.headline, "fn   in src  (*.rs)");
        assert_eq!(d.body, ToolBody::Text);
    }

    /// Every other tool — `task`, MCP calls — shows its arguments as `key: value`
    /// rows below the name, never raw JSON beside it.
    #[test]
    fn other_tools_show_their_args_as_detail_rows() {
        let d = tool_display(
            "task",
            r#"{"agent":"explore","description":"Explore the crate","prompt":"You are\n  an agent"}"#,
        );
        assert_eq!(d.headline, "", "nothing beside the tool name");
        assert_eq!(
            d.body,
            // Sorted by key: serde_json's Map is a BTreeMap here.
            ToolBody::Details(vec![
                ("agent".into(), "explore".into()),
                ("description".into(), "Explore the crate".into()),
                // Newlines and runs of spaces collapse: a row stays one row.
                ("prompt".into(), "You are an agent".into()),
            ])
        );

        // Non-string values render as compact JSON, not as Rust debug output.
        let d = tool_display("mcp__x__y", r#"{"a":1,"b":true,"c":[1,2],"d":null}"#);
        assert_eq!(
            d.body,
            ToolBody::Details(vec![
                ("a".into(), "1".into()),
                ("b".into(), "true".into()),
                ("c".into(), "[1,2]".into()),
                ("d".into(), "null".into()),
            ])
        );

        // No args at all: no rows, and nothing beside the name.
        let d = tool_display("mcp__x__y", "{}");
        assert_eq!(d.body, ToolBody::Details(vec![]));

        // A bare (non-object) argument becomes one unlabelled row.
        let d = tool_display("mcp__x__y", r#""just a string""#);
        assert_eq!(
            d.body,
            ToolBody::Details(vec![(String::new(), "just a string".into())])
        );

        // Malformed args must not panic: they parse as null, so no rows.
        assert_eq!(
            tool_display("mcp__x__y", "not json").body,
            ToolBody::Details(vec![])
        );
        assert_eq!(tool_display("write", "not json").headline, "?");
    }

    /// `todo` renders only its normalized replacement list from the result.
    #[test]
    fn todo_does_not_duplicate_its_input_items() {
        let d = tool_display(
            "todo",
            r#"{"todos":[{"content":"first","status":"completed"}]}"#,
        );
        assert_eq!(d.headline, "");
        assert_eq!(d.body, ToolBody::Text);
    }

    #[test]
    fn git_shows_subcommand_and_args_inline() {
        let d = tool_display(
            "git",
            r#"{"subcommand":"status","args":["--short","--branch"]}"#,
        );
        assert_eq!(d.headline, "status --short --branch");
        assert_eq!(d.body, ToolBody::Text);
    }

    #[test]
    fn git_without_args_shows_only_subcommand() {
        let d = tool_display("git", r#"{"subcommand":"log"}"#);
        assert_eq!(d.headline, "log");
        assert_eq!(d.body, ToolBody::Text);
    }

    #[test]
    fn git_with_empty_args_array_shows_only_subcommand() {
        let d = tool_display("git", r#"{"subcommand":"status","args":[]}"#);
        assert_eq!(d.headline, "status");
        assert_eq!(d.body, ToolBody::Text);
    }

    #[test]
    fn git_malformed_args_falls_back_to_empty_headline() {
        let d = tool_display("git", "not json");
        assert_eq!(d.headline, "");
        assert_eq!(d.body, ToolBody::Text);
    }

    #[test]
    fn git_no_args_falls_back_to_empty_headline() {
        let d = tool_display("git", "{}");
        assert_eq!(d.headline, "");
        assert_eq!(d.body, ToolBody::Text);
    }
}

#[cfg(test)]
mod apply_event_tests {
    use super::*;

    fn tool_start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            args: "{}".to_string(),
        }
    }

    #[test]
    fn text_coalesces_and_an_empty_delta_opens_nothing() {
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Text(String::new()));
        assert!(t.is_empty(), "an empty delta must not open an entry");
        apply_event(&mut t, &AgentEvent::Text("he".into()));
        apply_event(&mut t, &AgentEvent::Text("llo".into()));
        assert_eq!(t.len(), 1);
        assert!(matches!(&t[0].kind, EntryKind::Assistant(s) if s == "hello"));
    }

    #[test]
    fn a_tool_call_opens_streams_and_closes() {
        let mut t = Vec::new();
        apply_event(&mut t, &tool_start("c1", "bash"));
        apply_event(
            &mut t,
            &AgentEvent::ToolOutput {
                id: "c1".into(),
                chunk: "partial".into(),
            },
        );
        assert!(
            matches!(&t[0].kind, EntryKind::Tool { result, done: false, .. } if result == "partial")
        );
        apply_event(
            &mut t,
            &AgentEvent::ToolEnd {
                id: "c1".into(),
                name: "bash".into(),
                result: "final".into(),
                ok: false,
            },
        );
        assert!(
            matches!(&t[0].kind, EntryKind::Tool { result, done: true, ok: false, .. } if result == "final")
        );
    }

    #[test]
    fn a_steered_message_becomes_a_user_turn_in_the_pane() {
        // This is what makes a sub-agent view a conversation rather than a log:
        // what you send it shows up in its transcript, where you said it.
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Text("working".into()));
        apply_event(&mut t, &AgentEvent::Steered("actually, stop".into()));
        apply_event(&mut t, &AgentEvent::Text("ok".into()));
        let kinds: Vec<&EntryKind> = t.iter().map(|e| &e.kind).collect();
        assert!(matches!(kinds[0], EntryKind::Assistant(s) if s == "working"));
        assert!(matches!(kinds[1], EntryKind::User(s) if s == "actually, stop"));
        assert!(
            matches!(kinds[2], EntryKind::Assistant(s) if s == "ok"),
            "the reply after steering is a new block, not appended to the old one"
        );
    }

    #[test]
    fn reasoning_closes_when_anything_else_arrives() {
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Reasoning("hmm".into()));
        assert!(matches!(
            &t[0].kind,
            EntryKind::Reasoning { took_ms: None, .. }
        ));
        apply_event(&mut t, &AgentEvent::Text("answer".into()));
        assert!(
            matches!(
                &t[0].kind,
                EntryKind::Reasoning {
                    took_ms: Some(_),
                    ..
                }
            ),
            "the block is closed once the model moves on"
        );
    }

    /// An empty reasoning delta must not split a streamed reply into one block
    /// per chunk.
    ///
    /// Regression: a Qwen3-style backend keeps emitting `reasoning_content: ""`
    /// on every content chunk once it has stopped thinking (providers that omit
    /// the field deserialize to `None` and never reach here). Each empty delta
    /// used to push a `Reasoning` entry that rendered as nothing but sat at the
    /// tail of the transcript, so the next `Text` could not coalesce — the reply
    /// came out as `#8 assistant`, `#9 assistant`, … one header per token group,
    /// with the prose sliced across them.
    #[test]
    fn an_empty_reasoning_delta_does_not_split_the_reply() {
        let mut t = Vec::new();
        for fragment in ["I'll ", "audit ", "the codebase."] {
            apply_event(&mut t, &AgentEvent::Reasoning(String::new()));
            apply_event(&mut t, &AgentEvent::Text(fragment.into()));
        }
        assert_eq!(
            t.len(),
            1,
            "one streamed reply is one block, not one per chunk: {:?}",
            t.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        assert!(matches!(&t[0].kind, EntryKind::Assistant(s) if s == "I'll audit the codebase."));
    }

    /// The guard is on emptiness, not on reasoning: real reasoning still opens
    /// its own block and still breaks the assistant run, as before.
    #[test]
    fn a_non_empty_reasoning_delta_still_starts_its_own_block() {
        let mut t = Vec::new();
        apply_event(&mut t, &AgentEvent::Text("before".into()));
        apply_event(&mut t, &AgentEvent::Reasoning("thinking".into()));
        apply_event(&mut t, &AgentEvent::Text("after".into()));
        let kinds: Vec<&EntryKind> = t.iter().map(|e| &e.kind).collect();
        assert_eq!(kinds.len(), 3);
        assert!(matches!(kinds[0], EntryKind::Assistant(s) if s == "before"));
        assert!(matches!(kinds[1], EntryKind::Reasoning { text, .. } if text == "thinking"));
        assert!(matches!(kinds[2], EntryKind::Assistant(s) if s == "after"));
    }
}
