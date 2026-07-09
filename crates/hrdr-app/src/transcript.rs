//! The transcript data model shared by hrdr's frontends: the [`Entry`] enum (one
//! rendered item in the conversation) plus the representation-independent queries
//! over a slice of entries — search, message counting/indexing, and text/JSON
//! export. How an `Entry` is painted is the frontend's business; what counts as a
//! "message", how `/find` matches, and the export formats are shared here so the
//! TUI and GUI stay consistent.

use chrono::{DateTime, Local};
use hrdr_agent::{Message, MessageRole};

/// One rendered item in the transcript.
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
        /// Show the full result instead of a truncated preview (`/expand`).
        expanded: bool,
    },
    System(String),
    /// Final per-turn stats line, appended below the last output.
    Stats(String),
    /// A unified diff (e.g. `/diff`), rendered with diff coloring.
    Diff(String),
}

/// Extract the `command` field from a JSON tool-args string, if any.
/// Returns `None` for non-shell tools or malformed args.
pub fn extract_shell_command(name: &str, args: &str) -> Option<String> {
    if name != "bash" && name != "powershell" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(String::from))
}

impl Entry {
    /// The displayable text of a user/assistant message, if this entry is one.
    /// These are the only entries that count as numbered "messages" for `/find`,
    /// `/goto`, `/copy msg N`, and export.
    pub fn message_text(&self) -> Option<&str> {
        match self {
            Entry::User(s) | Entry::Assistant(s) => Some(s),
            _ => None,
        }
    }
}

/// Rebuild display entries from a restored message history (`/resume`, startup
/// auto-resume) — shared so the TUI and GUI reconstruct identically. User and
/// non-empty assistant texts become entries; each assistant `tool_calls` entry
/// is paired with its `role:"tool"` result by call id (the `Error:` prefix
/// convention marks a failed call). Other roles are skipped. Frontends map the
/// returned entries into their own representation (the TUI stores them as-is,
/// the GUI wraps each in its reactive signals).
pub fn messages_to_entries(msgs: &[Message]) -> Vec<Entry> {
    use std::collections::HashMap;
    // Map tool_call_id → (result, ok) from the tool-result messages.
    let mut results: HashMap<&str, (&str, bool)> = HashMap::new();
    for m in msgs {
        if m.role == MessageRole::Tool
            && let (Some(id), Some(content)) = (&m.tool_call_id, &m.content)
        {
            results.insert(id, (content, !content.starts_with("Error:")));
        }
    }
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            MessageRole::User => {
                if let Some(c) = &m.content {
                    out.push(Entry::User(c.clone()));
                }
            }
            MessageRole::Assistant => {
                if let Some(c) = &m.content
                    && !c.is_empty()
                {
                    out.push(Entry::Assistant(c.clone()));
                }
                for call in m.tool_calls.iter().flatten() {
                    let (result, ok) = results
                        .get(call.id.as_str())
                        .map(|(r, ok)| (r.to_string(), *ok))
                        .unwrap_or_default();
                    out.push(Entry::Tool {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        args: call.function.arguments.clone(),
                        result,
                        ok,
                        done: true,
                        expanded: false,
                    });
                }
            }
            _ => {}
        }
    }
    out
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
pub fn first_message_since(
    entries: &[Entry],
    times: &[DateTime<Local>],
    cutoff: DateTime<Local>,
) -> Option<usize> {
    let mut num = 0;
    for (i, e) in entries.iter().enumerate() {
        if e.message_text().is_some() {
            num += 1;
            if times.get(i).is_some_and(|t| *t >= cutoff) {
                return Some(num);
            }
        }
    }
    None
}

/// What a `/find`, `/next`, `/prev`, or `/goto` resolved to. The frontends
/// only differ in how they scroll — the parsing, cycling, wrap-around, and
/// status lines are all here.
#[derive(Debug, PartialEq, Eq)]
pub enum FindAction {
    /// Show this status line; nothing to scroll to.
    Info(String),
    /// Scroll message #`msg` into view and show `line`.
    Jump { msg: usize, line: String },
    /// Scroll to the very bottom (follow newest output) and show `line`.
    Bottom { line: String },
}

const GOTO_USAGE: &str = "usage: /goto <N | 5m | 1h | top | end>";

/// `/goto <N | 5m | 1h | top | end>` — resolve `arg` against a transcript with
/// `count` displayed messages. `first_since(cutoff)` returns the number of the
/// first message at/after that instant (the frontend's timestamp lookup).
pub fn goto_action(
    arg: &str,
    count: usize,
    first_since: impl FnOnce(DateTime<Local>) -> Option<usize>,
) -> FindAction {
    if count == 0 {
        return FindAction::Info("no messages to jump to yet".to_string());
    }
    let a = arg.trim().to_ascii_lowercase();
    let target = match a.as_str() {
        "" => return FindAction::Info(GOTO_USAGE.to_string()),
        "top" | "start" | "first" => 1,
        "end" | "bottom" | "last" => {
            return FindAction::Bottom {
                line: "jumped to the latest output".to_string(),
            };
        }
        _ => {
            if let Ok(n) = a.parse::<usize>() {
                n.clamp(1, count)
            } else if let Some(secs) = crate::parse_duration(&a) {
                let cutoff = Local::now() - chrono::Duration::seconds(secs);
                // First message at/after the cutoff; if all are older, the
                // newest one is closest to "that long ago".
                first_since(cutoff).unwrap_or(count)
            } else {
                return FindAction::Info(GOTO_USAGE.to_string());
            }
        }
    };
    FindAction::Jump {
        msg: target,
        line: format!("jumped to message #{target}"),
    }
}

/// The `/find` / `/next` / `/prev` state machine: active query + last-visited
/// message number. Both frontends hold one (the GUI behind signals) and route
/// the returned [`FindAction`] to their scroll primitive.
#[derive(Debug, Default, Clone)]
pub struct FindState {
    /// The active query, if a search is live (also drives match highlighting).
    pub query: Option<String>,
    /// Message number of the last-visited match (0 = none yet).
    pub pos: usize,
}

impl FindState {
    /// `/find <text>` — start/restart a search and jump to the first match;
    /// no arg re-cycles the active query; `clear`/`off`/`discard` drops it.
    /// `hits(query)` returns the matching message numbers, ascending.
    pub fn find(&mut self, arg: &str, hits: impl FnOnce(&str) -> Vec<usize>) -> FindAction {
        if matches!(
            arg.trim().to_ascii_lowercase().as_str(),
            "clear" | "off" | "discard"
        ) {
            return if self.query.take().is_some() {
                self.pos = 0;
                FindAction::Info("search cleared".to_string())
            } else {
                FindAction::Info("no active search".to_string())
            };
        }
        let arg = arg.trim();
        if arg.is_empty() {
            if self.query.is_none() {
                return FindAction::Info("usage: /find <text>".to_string());
            }
        } else {
            // A new query restarts cycling from the top.
            if self.query.as_deref() != Some(arg) {
                self.pos = 0;
            }
            self.query = Some(arg.to_string());
        }
        self.cycle(true, hits)
    }

    /// `/next` / `/prev` — advance to the next (`forward`) or previous match
    /// of the active query, wrapping around.
    pub fn cycle(&mut self, forward: bool, hits: impl FnOnce(&str) -> Vec<usize>) -> FindAction {
        let Some(query) = self.query.clone() else {
            return FindAction::Info("no active search — /find <text>".to_string());
        };
        let hits = hits(&query);
        if hits.is_empty() {
            return FindAction::Info(format!("no match for {query:?}"));
        }
        let target = if forward {
            hits.iter()
                .copied()
                .find(|&n| n > self.pos)
                .unwrap_or(hits[0])
        } else {
            hits.iter()
                .rev()
                .copied()
                .find(|&n| n < self.pos)
                .unwrap_or(*hits.last().unwrap())
        };
        let idx = hits.iter().position(|&n| n == target).unwrap_or(0) + 1;
        self.pos = target;
        FindAction::Jump {
            msg: target,
            line: format!(
                "match {idx}/{} for {query:?} → message #{target}",
                hits.len()
            ),
        }
    }
}

/// The transcript as Markdown-ish text (user/assistant/system/diff/tool lines;
/// reasoning and stats are omitted). Used by `/copy all` and `/export`.
pub fn transcript_to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e {
            Entry::User(s) => out.push_str(&format!("## User\n{s}\n\n")),
            Entry::Assistant(s) => out.push_str(&format!("## Assistant\n{s}\n\n")),
            Entry::System(s) => out.push_str(&format!("[{s}]\n\n")),
            Entry::Diff(s) => out.push_str(&format!("{s}\n\n")),
            Entry::Tool { name, .. } => out.push_str(&format!("[tool: {name}]\n\n")),
            Entry::Reasoning(_) | Entry::Stats(_) => {}
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            Entry::System("welcome".into()),
            Entry::User("Fix the parser bug".into()),
            Entry::Reasoning("thinking…".into()),
            Entry::Assistant("Done — it was an off-by-one.".into()),
            Entry::User("thanks".into()),
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

    #[test]
    fn goto_action_parses_targets() {
        let none = |_| None;
        assert_eq!(
            goto_action("", 0, none),
            FindAction::Info("no messages to jump to yet".to_string())
        );
        assert!(matches!(
            goto_action("", 5, none),
            FindAction::Info(l) if l.starts_with("usage:")
        ));
        assert!(matches!(
            goto_action("top", 5, none),
            FindAction::Jump { msg: 1, .. }
        ));
        assert!(matches!(
            goto_action("end", 5, none),
            FindAction::Bottom { .. }
        ));
        // Numbers clamp to the message count.
        assert!(matches!(
            goto_action("99", 5, none),
            FindAction::Jump { msg: 5, .. }
        ));
        // Durations resolve through the frontend's timestamp lookup; all-older
        // falls back to the newest message.
        assert!(matches!(
            goto_action("5m", 7, |_| Some(3)),
            FindAction::Jump { msg: 3, .. }
        ));
        assert!(matches!(
            goto_action("5m", 7, |_| None),
            FindAction::Jump { msg: 7, .. }
        ));
        assert!(matches!(
            goto_action("garbage", 5, none),
            FindAction::Info(l) if l.starts_with("usage:")
        ));
    }

    #[test]
    fn find_state_cycles_and_wraps() {
        let hits = |q: &str| if q == "x" { vec![2, 4, 7] } else { vec![] };
        let mut st = FindState::default();
        // No active query yet.
        assert!(matches!(st.cycle(true, hits), FindAction::Info(_)));
        // New query jumps to the first match.
        assert!(matches!(
            st.find("x", hits),
            FindAction::Jump { msg: 2, .. }
        ));
        assert!(matches!(
            st.cycle(true, hits),
            FindAction::Jump { msg: 4, .. }
        ));
        assert!(matches!(
            st.cycle(true, hits),
            FindAction::Jump { msg: 7, .. }
        ));
        // Wraps forward…
        assert!(matches!(
            st.cycle(true, hits),
            FindAction::Jump { msg: 2, .. }
        ));
        // …and backward.
        assert!(matches!(
            st.cycle(false, hits),
            FindAction::Jump { msg: 7, .. }
        ));
        // Bare /find re-cycles the active query.
        assert!(matches!(st.find("", hits), FindAction::Jump { msg: 2, .. }));
        // Repeating the same query keeps cycling from the current position…
        st.pos = 7;
        assert!(matches!(
            st.find("x", hits),
            FindAction::Jump { msg: 2, .. }
        ));
        // …while a changed query restarts from the top (and here finds nothing).
        assert!(matches!(st.find("y", hits), FindAction::Info(l) if l.contains("no match")));
        assert_eq!(st.pos, 0);
        // Clear drops the query; clearing again reports no search.
        st.query = Some("x".to_string());
        assert!(matches!(st.find("clear", hits), FindAction::Info(l) if l == "search cleared"));
        assert!(st.query.is_none() && st.pos == 0);
        assert!(matches!(st.find("off", hits), FindAction::Info(l) if l == "no active search"));
    }
}
