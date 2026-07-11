//! The interactive `/resume` picker: every saved session, newest first, shown
//! in columns (id · name · age · cwd) and narrowed by a fuzzy-find query typed
//! into its input box. Same shape (and chrome) as the `/model` selector.

use hrdr_app::{SessionMeta, filter_sessions};

pub(crate) struct SessionSelector {
    /// All sessions, newest first (as [`hrdr_app::list_sessions`] returns them).
    sessions: Vec<SessionMeta>,
    /// The fuzzy-find query (case-insensitive against id + name + cwd).
    pub(crate) filter: String,
    /// Indices into `sessions` matching `filter`, newest first.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
}

impl SessionSelector {
    pub(crate) fn new(sessions: Vec<SessionMeta>) -> Self {
        let filtered = (0..sessions.len()).collect();
        Self {
            sessions,
            filter: String::new(),
            filtered,
            selected: 0,
        }
    }

    fn refilter(&mut self) {
        self.filtered = filter_sessions(&self.sessions, &self.filter);
        self.selected = 0;
    }

    pub(crate) fn push_char(&mut self, c: char) {
        self.filter.push(c);
        self.refilter();
    }

    pub(crate) fn backspace(&mut self) {
        self.filter.pop();
        self.refilter();
    }

    pub(crate) fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// The filtered sessions in display order (newest first).
    pub(crate) fn rows(&self) -> impl Iterator<Item = &SessionMeta> {
        self.filtered.iter().map(move |&i| &self.sessions[i])
    }

    /// The currently-highlighted session, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&SessionMeta> {
        self.filtered.get(self.selected).map(|&i| &self.sessions[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, name: &str, cwd: &str, updated: u64) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            name: name.to_string(),
            cwd: cwd.to_string(),
            updated,
            path: std::path::PathBuf::from(format!("/tmp/{id}.json")),
        }
    }

    #[test]
    fn filter_navigate_and_select() {
        let mut s = SessionSelector::new(vec![
            meta("newest", "TUI polish", "/home/u/hrdr", 30),
            meta("older", "Fix the auth bug", "/home/u/api", 20),
            meta("oldest", "Auth follow-up", "/home/u/api", 10),
        ]);
        // Input order is preserved (newest first).
        assert_eq!(s.current().unwrap().id, "newest");
        s.down();
        assert_eq!(s.current().unwrap().id, "older");
        s.up();
        s.up();
        assert_eq!(s.current().unwrap().id, "newest");

        // Typing filters against id + name + cwd and resets the highlight.
        for c in "auth".chars() {
            s.push_char(c);
        }
        assert_eq!(s.rows().count(), 2);
        assert_eq!(s.current().unwrap().id, "older");

        // Backspacing widens the match again.
        for _ in 0.."auth".len() {
            s.backspace();
        }
        assert_eq!(s.rows().count(), 3);

        // A filter matching nothing leaves no current selection.
        for c in "zzz".chars() {
            s.push_char(c);
        }
        assert!(s.current().is_none());
    }
}
