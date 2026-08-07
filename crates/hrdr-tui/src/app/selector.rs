//! The pickers' shared state machine: a filterable, navigable list. Every
//! picker modal (`/model`, `/resume`, `/theme`, `/effort`, `/skills`, the
//! `/login` provider list) is a `Selector<T>` over its own choice type with a
//! fuzzy filter function; only what Enter *does* with the highlighted choice
//! differs, and that lives in each picker's key handler.

use hrdr_agent::{ModelChoice, filter_model_choices, model_choice_haystack};
use hrdr_app::{
    EffortChoice, LoginProviderChoice, SessionMeta, Skill, ThemeChoice, effort_choice_haystack,
    filter_effort_choices, filter_login_providers, filter_sessions, filter_skills, filter_themes,
    login_provider_haystack, session_haystack, skill_haystack, theme_choice_haystack,
};

pub(crate) struct Selector<T> {
    /// All choices, in the order the picker's data source produced them.
    choices: Vec<T>,
    /// Lowercase search haystack per choice, built once when the picker opens
    /// (or the choices are replaced) — a keystroke's refilter then walks
    /// precomputed text instead of re-deriving every choice's haystack.
    haystacks: Vec<String>,
    /// The fuzzy-find query typed into the picker's search line.
    pub(crate) filter: String,
    /// Indices into `choices` matching `filter`, in input order.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
    /// The picker's fuzzy filter (matching indices for a query).
    filter_fn: fn(&[String], &str) -> Vec<usize>,
}

impl<T> Selector<T> {
    pub(crate) fn new(
        choices: Vec<T>,
        haystack_fn: fn(&T) -> String,
        filter_fn: fn(&[String], &str) -> Vec<usize>,
    ) -> Self {
        let haystacks = choices.iter().map(haystack_fn).collect();
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            haystacks,
            filter: String::new(),
            filtered,
            selected: 0,
            filter_fn,
        }
    }

    fn refilter(&mut self) {
        self.filtered = (self.filter_fn)(&self.haystacks, &self.filter);
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
    pub(crate) fn rows(&self) -> impl Iterator<Item = &T> {
        self.filtered.iter().map(move |&i| &self.choices[i])
    }

    /// The currently-highlighted choice, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&T> {
        self.filtered.get(self.selected).map(|&i| &self.choices[i])
    }
}

/// The keys every picker shares: Esc/Ctrl+C close, Up/Down/Backspace move the
/// highlight, and any other non-ctrl char edits the fuzzy filter. Runs the
/// shared part and reports what kind of key it was so each handler keeps only
/// its divergent bits (Enter, Ctrl+D, the theme preview, the skill insert).
pub(super) enum SelectorKey {
    /// A shared key, fully handled (highlight move or filter edit).
    Handled,
    /// Esc or Ctrl+C — the caller closes its own picker (the close differs:
    /// some restore state, some just drop the selector).
    Close,
    /// Anything else — the caller decides (Enter applies, Ctrl+D persists …).
    Other(crossterm::event::KeyEvent),
}

/// Dispatch the keys every picker shares, mutating the selector for the shared
/// ones, and report which way the key went so the caller keeps only its
/// divergent arms (Enter, Ctrl+D, the theme preview, the skill insert).
pub(super) fn selector_key<T>(
    sel: &mut Option<Selector<T>>,
    key: crossterm::event::KeyEvent,
) -> SelectorKey {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => SelectorKey::Close,
        KeyCode::Char('c') if ctrl => SelectorKey::Close,
        KeyCode::Up => {
            if let Some(s) = sel {
                s.up();
            }
            SelectorKey::Handled
        }
        KeyCode::Down => {
            if let Some(s) = sel {
                s.down();
            }
            SelectorKey::Handled
        }
        KeyCode::Backspace => {
            if let Some(s) = sel {
                s.backspace();
            }
            SelectorKey::Handled
        }
        KeyCode::Char(ch) if !ctrl => {
            if let Some(s) = sel {
                s.push_char(ch);
            }
            SelectorKey::Handled
        }
        _ => SelectorKey::Other(key),
    }
}

// ── Per-picker aliases + constructors (each pairs a choice type with its
//    fuzzy filter) ──────────────────────────────────────────────────────────

pub(crate) type ModelSelector = Selector<ModelChoice>;
pub(crate) fn model_selector(choices: Vec<ModelChoice>) -> ModelSelector {
    Selector::new(choices, model_choice_haystack, filter_model_choices)
}

impl Selector<ModelChoice> {
    /// Replace the choice list (e.g. after an async ChatGPT catalog load),
    /// preserving the typed filter and re-selecting the same `(provider, model)`
    /// when it survives — else clamping the highlight to the top.
    pub(crate) fn replace_model_choices(&mut self, choices: Vec<ModelChoice>) {
        let current = self
            .current()
            .map(|c| (c.provider.clone(), c.model.clone()));
        self.haystacks = choices.iter().map(model_choice_haystack).collect();
        self.choices = choices;
        self.filtered = (self.filter_fn)(&self.haystacks, &self.filter);
        self.selected = current
            .and_then(|(p, m)| {
                self.filtered
                    .iter()
                    .position(|&i| self.choices[i].provider == p && self.choices[i].model == m)
            })
            .unwrap_or(0);
    }
}

pub(crate) type SessionSelector = Selector<SessionMeta>;
pub(crate) fn session_selector(sessions: Vec<SessionMeta>) -> SessionSelector {
    Selector::new(sessions, session_haystack, filter_sessions)
}

pub(crate) type ThemeSelector = Selector<ThemeChoice>;
pub(crate) fn theme_selector(choices: Vec<ThemeChoice>) -> ThemeSelector {
    Selector::new(choices, theme_choice_haystack, filter_themes)
}

pub(crate) type EffortSelector = Selector<EffortChoice>;
pub(crate) fn effort_selector(choices: Vec<EffortChoice>) -> EffortSelector {
    Selector::new(choices, effort_choice_haystack, filter_effort_choices)
}

pub(crate) type SkillSelector = Selector<Skill>;
pub(crate) fn skill_selector(skills: Vec<Skill>) -> SkillSelector {
    Selector::new(skills, skill_haystack, filter_skills)
}

pub(crate) type LoginProviderSelector = Selector<LoginProviderChoice>;
pub(crate) fn login_provider_selector(choices: Vec<LoginProviderChoice>) -> LoginProviderSelector {
    Selector::new(choices, login_provider_haystack, filter_login_providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared machine: filter narrows (resetting the highlight), Up/Down
    /// clamp, an empty filter restores everything, and no match → no current.
    #[test]
    fn filter_navigate_and_select() {
        let choices =
            hrdr_app::choices_from(&["low".to_string(), "medium".to_string(), "high".to_string()]);
        let mut s = effort_selector(choices);
        assert_eq!(s.current().unwrap().label, "Default");
        s.up(); // clamps at the top
        assert_eq!(s.current().unwrap().label, "Default");
        s.down();
        assert_eq!(s.current().unwrap().label, "High");

        for c in "medium".chars() {
            s.push_char(c);
        }
        assert_eq!(s.rows().count(), 1);
        assert_eq!(s.current().unwrap().value.as_deref(), Some("medium"));

        s.push_char('z');
        assert!(s.current().is_none());
        s.backspace();
        assert_eq!(s.current().unwrap().label, "Medium");
    }

    fn mc(provider: &str, model: &str, label: &str) -> ModelChoice {
        ModelChoice {
            provider: provider.to_string(),
            model: model.to_string(),
            provider_label: provider.to_string(),
            model_label: label.to_string(),
            context_window: None,
        }
    }

    /// An async catalog load replaces the rows but keeps the user's filter and
    /// re-selects the same model when it survives (the filter is a subsequence
    /// match over "label provider", so labels are chosen to isolate matches).
    #[test]
    fn replace_model_choices_preserves_filter_and_selection() {
        let mut s = model_selector(vec![
            mc("zen", "a", "Alpha"),
            mc("zen", "o", "Cobra"),
            mc("zen", "c", "Cappa"),
        ]);
        // Filter 'c' matches Cobra + Cappa (not Alpha); select Cappa.
        s.push_char('c');
        assert_eq!(s.rows().count(), 2);
        s.down();
        assert_eq!(s.current().unwrap().model, "c");

        // A new list (reordered, extra row) arrives.
        s.replace_model_choices(vec![
            mc("zen", "x", "Xray"),
            mc("zen", "c", "Cappa"),
            mc("zen", "a", "Alpha"),
        ]);
        // The filter is retained and Cappa stays selected.
        assert_eq!(s.filter, "c");
        assert_eq!(s.current().unwrap().model, "c");
    }

    /// When the selected model vanishes from the new list, the highlight clamps
    /// to the top of the (unfiltered) list.
    #[test]
    fn replace_model_choices_clamps_when_selection_vanishes() {
        let mut s = model_selector(vec![mc("zen", "a", "Alpha"), mc("zen", "o", "Cobra")]);
        s.down(); // select Cobra
        assert_eq!(s.current().unwrap().model, "o");
        s.replace_model_choices(vec![mc("zen", "z", "Zeta"), mc("zen", "a", "Alpha")]);
        assert_eq!(s.current().unwrap().model, "z", "clamps to the first row");
    }
}
