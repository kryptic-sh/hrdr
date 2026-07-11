//! The interactive `/effort` picker: the reasoning levels the current model
//! actually accepts (models.dev catalog), highest effort first, with a
//! "Default" row on top that clears the override. Same shape (and chrome) as
//! the other pickers, narrowed by a fuzzy-find query.

use hrdr_app::{EffortChoice, filter_effort_choices};

pub(crate) struct EffortSelector {
    /// All choices: "Default" first, then highest → lowest effort.
    choices: Vec<EffortChoice>,
    /// The fuzzy-find query (case-insensitive against label + value + detail).
    pub(crate) filter: String,
    /// Indices into `choices` matching `filter`, in input order.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
}

impl EffortSelector {
    pub(crate) fn new(choices: Vec<EffortChoice>) -> Self {
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            filter: String::new(),
            filtered,
            selected: 0,
        }
    }

    fn refilter(&mut self) {
        self.filtered = filter_effort_choices(&self.choices, &self.filter);
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

    /// The filtered choices in display order.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &EffortChoice> {
        self.filtered.iter().map(move |&i| &self.choices[i])
    }

    /// The currently-highlighted choice, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&EffortChoice> {
        self.filtered.get(self.selected).map(|&i| &self.choices[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_navigate_and_select() {
        let mut s = EffortSelector::new(hrdr_app::choices_from(&[
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]));
        // Default leads, then highest effort first.
        assert_eq!(s.current().unwrap().label, "Default");
        s.down();
        assert_eq!(s.current().unwrap().label, "High");

        // Typing filters and resets the highlight ("medium", not "med" — the
        // subsequence filter would also keep Default via "ModEl/proviDer").
        for c in "medium".chars() {
            s.push_char(c);
        }
        assert_eq!(s.rows().count(), 1);
        assert_eq!(s.current().unwrap().value.as_deref(), Some("medium"));

        // A filter matching nothing leaves no current selection.
        s.push_char('z');
        assert!(s.current().is_none());
        s.backspace();
        assert_eq!(s.current().unwrap().label, "Medium");
    }
}
