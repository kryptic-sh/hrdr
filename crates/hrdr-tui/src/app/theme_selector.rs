//! The interactive `/theme` picker: the baked-in themes plus any
//! `~/.config/hrdr/themes/*.toml`, shown in two columns (name · source) and
//! narrowed by a fuzzy-find query. Same shape (and chrome) as the `/model`
//! picker, plus live preview: the highlighted theme is applied as you move,
//! and Esc restores the theme that was in force when the picker opened.

use hrdr_app::{ThemeChoice, filter_themes};

pub(crate) struct ThemeSelector {
    /// All choices: built-ins first (default leading), then user themes.
    choices: Vec<ThemeChoice>,
    /// The fuzzy-find query (case-insensitive against name + source).
    pub(crate) filter: String,
    /// Indices into `choices` matching `filter`, in input order.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
    /// The theme in force when the picker opened — restored on Esc (and while
    /// no row matches the filter).
    pub(crate) original: crate::theme::Theme,
}

impl ThemeSelector {
    pub(crate) fn new(choices: Vec<ThemeChoice>, original: crate::theme::Theme) -> Self {
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            filter: String::new(),
            filtered,
            selected: 0,
            original,
        }
    }

    fn refilter(&mut self) {
        self.filtered = filter_themes(&self.choices, &self.filter);
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
    pub(crate) fn rows(&self) -> impl Iterator<Item = &ThemeChoice> {
        self.filtered.iter().map(move |&i| &self.choices[i])
    }

    /// The currently-highlighted choice, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&ThemeChoice> {
        self.filtered.get(self.selected).map(|&i| &self.choices[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(name: &str) -> ThemeChoice {
        ThemeChoice {
            name: name.to_string(),
            spec: name.to_string(),
            source: "built-in".to_string(),
        }
    }

    #[test]
    fn filter_navigate_and_select() {
        let mut s = ThemeSelector::new(
            vec![choice("tokyonight"), choice("dracula"), choice("nord")],
            crate::theme::Theme::default(),
        );
        assert_eq!(s.current().unwrap().name, "tokyonight");
        s.down();
        assert_eq!(s.current().unwrap().name, "dracula");

        // Typing filters and resets the highlight to the top.
        for c in "nord".chars() {
            s.push_char(c);
        }
        assert_eq!(s.rows().count(), 1);
        assert_eq!(s.current().unwrap().name, "nord");

        // A filter matching nothing leaves no current selection.
        s.push_char('z');
        assert!(s.current().is_none());
        s.backspace();
        assert_eq!(s.current().unwrap().name, "nord");
    }
}
