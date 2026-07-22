//! Frontend-facing transcript helpers layered on the shared model.
//!
//! The transcript data model — [`Entry`], [`EntryKind`], the constructors, the
//! search/count/export queries, and tool-display classification — now lives in
//! [`hrdr_agent`] so both the main and sub-agent recording paths share it. This
//! module re-exports those items (so existing `crate::` paths keep resolving)
//! and keeps only the `/goto` resolver and the `/find` state machine, which
//! depend on hrdr-app's [`crate::parse_duration`].

pub use hrdr_agent::{
    Entry, EntryKind, ToolBody, ToolDisplay, extract_shell_command, find_hits,
    first_message_since, message_count, nth_message_text, settle_restored_tools, time_from_system,
    time_from_unix, tool_display, transcript_to_text,
};

use chrono::{DateTime, Local};

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
/// message number. A frontend holds one and routes the returned [`FindAction`]
/// to its scroll primitive.
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

#[cfg(test)]
mod tests {
    use super::*;

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
