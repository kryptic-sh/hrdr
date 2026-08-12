//! Frontend-facing transcript helpers layered on the shared model.
//!
//! The transcript data model — [`Entry`], [`EntryKind`], the constructors, the
//! search/count/export queries, and tool-display classification — now lives in
//! [`hrdr_agent`] so both the main and sub-agent recording paths share it. This
//! module re-exports those items (so existing `crate::` paths keep resolving)
//! and keeps only the `/find` state machine.

pub use hrdr_agent::{
    Entry, EntryKind, ToolBody, ToolDisplay, apply_event, extract_shell_command, find_hits,
    settle_restored_entries, time_from_system, time_from_unix, tool_display, transcript_to_text,
};

/// What a `/find`, `/next`, or `/prev` resolved to: a status line to show, or
/// a jump to message #`msg` (and its status line). The frontends only differ
/// in how they scroll.
#[derive(Debug, PartialEq, Eq)]
pub enum FindAction {
    /// Show this status line; nothing to scroll to.
    Info(String),
    /// Scroll message #`msg` into view and show `line`.
    Jump { msg: usize, line: String },
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
