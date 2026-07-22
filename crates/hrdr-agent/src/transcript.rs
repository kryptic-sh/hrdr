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
