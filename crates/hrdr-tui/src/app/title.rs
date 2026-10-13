//! The terminal title: what the session is doing, where, and under what name,
//! readable at a glance from a tab bar or a taskbar.
//!
//! `<status> <cwd basename> · <session name>`, the status an emoji leading the
//! title so it survives truncation to a narrow tab.

use super::*;

/// What a glance at the title should tell the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TitleStatus {
    /// A turn, a compaction or a `!command` is running.
    Working,
    /// The session is blocked on the user: a prompt is open, or the last turn
    /// ended on a question.
    Waiting,
    /// A turn finished and the user has not touched the session since.
    Done,
    /// Nothing is running and nothing is new.
    Idle,
}

impl TitleStatus {
    fn emoji(self) -> &'static str {
        match self {
            Self::Working => "⏳",
            Self::Waiting => "❓",
            Self::Done => "✅",
            Self::Idle => "💤",
        }
    }
}

impl App {
    pub(crate) fn title_status(&self) -> TitleStatus {
        let shell_running = self
            .user_shell
            .as_ref()
            .is_some_and(|shell| !shell.handle.is_finished());
        if self.running() || self.compacting() || shell_running {
            TitleStatus::Working
        } else if self.login_modal.is_some()
            || self.passive_recovery.is_some()
            || (self.turn_unseen && self.last_reply_asks())
        {
            TitleStatus::Waiting
        } else if self.turn_unseen {
            TitleStatus::Done
        } else {
            TitleStatus::Idle
        }
    }

    /// Whether the conversation's last word is the agent's, and a question.
    fn last_reply_asks(&self) -> bool {
        self.panes
            .main()
            .transcript()
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::Assistant(text) => Some(text.trim_end().ends_with('?')),
                EntryKind::User(_) => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    pub(crate) fn terminal_title(&self) -> String {
        let place = title_text(
            self.dir_cwd
                .file_name()
                .map(|name| name.to_string_lossy())
                .as_deref()
                .unwrap_or_default(),
        );
        let name = title_text(&self.state().name);
        let label = match (place.is_empty(), name.is_empty()) {
            (false, false) => format!("{place} · {name}"),
            (false, true) => place,
            (true, false) => name,
            (true, true) => "hrdr".to_string(),
        };
        format!("{} {label}", self.title_status().emoji())
    }

    /// The user is looking: a finished turn is no longer news.
    pub(crate) fn note_user_input(&mut self) {
        self.turn_unseen = false;
    }
}

/// Text fit for a title: no control characters (an escape sequence in a
/// session name must not reach the terminal), no surrounding whitespace.
fn title_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::title_text;

    #[test]
    fn title_text_drops_control_sequences() {
        assert_eq!(title_text("ship\u{1b}]0;spoof\u{7}"), "ship]0;spoof");
        assert_eq!(title_text("  release work  "), "release work");
        assert_eq!(title_text("\n\t"), "");
    }
}
