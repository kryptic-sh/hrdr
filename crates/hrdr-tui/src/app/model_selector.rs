//! The interactive `/model` selector: every model across the configured
//! providers, shown in two columns (friendly model name · friendly provider
//! name) and narrowed by a fuzzy-find query typed into its input box.

use hrdr_agent::{ModelChoice, filter_model_choices};

pub(crate) struct ModelSelector {
    /// All choices, pre-sorted by (model, provider).
    choices: Vec<ModelChoice>,
    /// The fuzzy-find query (case-insensitive against model + provider names).
    pub(crate) filter: String,
    /// Indices into `choices` matching `filter`, in sorted order.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
}

impl ModelSelector {
    pub(crate) fn new(choices: Vec<ModelChoice>) -> Self {
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            filter: String::new(),
            filtered,
            selected: 0,
        }
    }

    fn refilter(&mut self) {
        self.filtered = filter_model_choices(&self.choices, &self.filter);
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
    pub(crate) fn rows(&self) -> impl Iterator<Item = &ModelChoice> {
        self.filtered.iter().map(move |&i| &self.choices[i])
    }

    /// The currently-highlighted choice, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&ModelChoice> {
        self.filtered.get(self.selected).map(|&i| &self.choices[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(model: &str, provider: &str) -> ModelChoice {
        ModelChoice {
            provider: provider.to_string(),
            model: model.to_string(),
            provider_label: provider.to_string(),
            model_label: model.to_string(),
        }
    }

    #[test]
    fn filter_navigate_and_select() {
        let mut s = ModelSelector::new(vec![
            choice("Claude Fable 5.0", "OpenCode Zen"),
            choice("DeepSeek V4 Pro", "OpenCode Go"),
            choice("GPT-5.6", "OpenCode Zen"),
        ]);
        // Down moves the highlight; Enter would take the second row.
        s.down();
        assert_eq!(s.current().unwrap().model_label, "DeepSeek V4 Pro");
        // Up clamps at the top.
        s.up();
        s.up();
        assert_eq!(s.current().unwrap().model_label, "Claude Fable 5.0");

        // Typing filters (case-insensitive) and resets the highlight to the top.
        for c in "zen".chars() {
            s.push_char(c);
        }
        assert_eq!(s.filter, "zen");
        // Both Zen models survive; the highlight is back at the first.
        assert_eq!(s.rows().count(), 2);
        assert_eq!(s.current().unwrap().provider_label, "OpenCode Zen");

        // Backspacing widens the match again.
        s.backspace();
        s.backspace();
        s.backspace();
        assert_eq!(s.rows().count(), 3);

        // A filter matching nothing leaves no current selection.
        for c in "zzz".chars() {
            s.push_char(c);
        }
        assert!(s.current().is_none());
    }
}
