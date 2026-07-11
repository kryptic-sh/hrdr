//! Bundled themes + the `/theme` picker's choice list, shared by hrdr's
//! frontends. Five popular palettes ship baked into the binary (Tokyo Night —
//! the default — plus Catppuccin Mocha, Dracula, Gruvbox Dark, and Nord), and
//! user themes are picked up from `~/.config/hrdr/themes/*.toml`. A theme
//! "spec" — what `/theme <spec>` accepts and what the config `theme` key
//! stores — is either a built-in name or a file path.

use std::path::PathBuf;

/// The baked-in themes, as `(name, theme TOML)`. The first entry is the
/// default theme (Tokyo Night).
pub const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("tokyonight", include_str!("../themes/tokyonight.toml")),
    (
        "catppuccin-mocha",
        include_str!("../themes/catppuccin-mocha.toml"),
    ),
    ("dracula", include_str!("../themes/dracula.toml")),
    ("gruvbox-dark", include_str!("../themes/gruvbox-dark.toml")),
    ("nord", include_str!("../themes/nord.toml")),
];

/// The TOML source of a baked-in theme by name (trimmed, case-insensitive).
pub fn builtin_theme_toml(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    BUILTIN_THEMES
        .iter()
        .find(|(name, _)| *name == n)
        .map(|(_, toml)| *toml)
}

/// Where user theme files live: `$XDG_CONFIG_HOME/hrdr/themes` (default
/// `~/.config/hrdr/themes`). Each `*.toml` inside is offered by the picker
/// under its file stem.
pub fn user_themes_dir() -> Option<PathBuf> {
    Some(hjkl_xdg::config_dir("hrdr").ok()?.join("themes"))
}

/// One pickable theme: the display name, the spec to apply/persist (built-in
/// name or file path), and a short source label for the picker's second column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeChoice {
    /// Display name (built-in name, or the file stem for user themes).
    pub name: String,
    /// What `/theme <spec>` applies and the config `theme` key stores.
    pub spec: String,
    /// Where it comes from: `built-in` or the theme file's directory.
    pub source: String,
}

/// Every theme the user can pick: the baked-in five (default first), then any
/// `*.toml` under [`user_themes_dir`], sorted by name. A user theme whose stem
/// collides with a built-in name still appears — its path spec keeps the two
/// distinct.
pub fn theme_choices() -> Vec<ThemeChoice> {
    let mut out: Vec<ThemeChoice> = BUILTIN_THEMES
        .iter()
        .map(|(name, _)| ThemeChoice {
            name: (*name).to_string(),
            spec: (*name).to_string(),
            source: "built-in".to_string(),
        })
        .collect();
    let mut user: Vec<ThemeChoice> = user_themes_dir()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "toml") {
                return None;
            }
            Some(ThemeChoice {
                name: path.file_stem()?.to_string_lossy().into_owned(),
                spec: path.display().to_string(),
                source: crate::display_dir(path.parent()?),
            })
        })
        .collect();
    user.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(user);
    out
}

/// Case-insensitive fuzzy filter over theme choices: the query's characters
/// must appear in order within `"name source"`. Returns matching indices in
/// input order; an empty query matches everything.
pub fn filter_themes(choices: &[ThemeChoice], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..choices.len()).collect();
    }
    choices
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = format!("{} {}", c.name, c.source).to_lowercase();
            crate::is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bundled theme must parse — `load_hjkl_theme` swallows a parse
    /// error and silently falls back to the default palette, so a typo in a
    /// bundled TOML would ship the wrong colors with no warning.
    ///
    /// Regression (from the Tokyo Night file): `[palette]` values must be
    /// literal hex — only `[ui]` resolves `$refs`.
    #[test]
    fn every_bundled_theme_parses() {
        for (name, toml) in BUILTIN_THEMES {
            hjkl_theme::Theme::from_toml_str(toml)
                .unwrap_or_else(|e| panic!("bundled theme {name} must parse: {e}"));
        }
    }

    /// Each bundled theme resolves the roles hrdr actually renders with —
    /// nothing silently falls through to a frontend fallback color.
    #[test]
    fn every_bundled_theme_covers_the_chat_roles() {
        for (name, toml) in BUILTIN_THEMES {
            let t = hjkl_theme::Theme::from_toml_str(toml).unwrap();
            let p = crate::ChatPalette::from_hjkl(&t);
            for (role, v) in [
                ("background", p.background),
                ("user", p.user),
                ("user_bg", p.user_bg),
                ("assistant", p.assistant),
                ("dim", p.dim),
                ("warn", p.warn),
                ("command_bg", p.command_bg),
                ("stats_bg", p.stats_bg),
                ("prompt_border", p.prompt_border),
                ("success", p.success),
                ("error", p.error),
                ("accent", p.accent),
                ("accent2", p.accent2),
            ] {
                assert!(v.is_some(), "{name}: role {role} unresolved");
            }
            // The block backgrounds must be tellable apart (the transcript
            // relies on it), and the two accents must differ.
            assert_ne!(p.user_bg, p.command_bg, "{name}: user vs command bg");
            assert_ne!(p.accent, p.accent2, "{name}: accent2 fell back to accent");
        }
    }

    #[test]
    fn builtin_lookup_is_case_insensitive_and_choices_start_with_the_default() {
        assert!(builtin_theme_toml("TokyoNight").is_some());
        assert!(builtin_theme_toml(" nord ").is_some());
        assert!(builtin_theme_toml("no-such-theme").is_none());
        let choices = theme_choices();
        assert_eq!(choices[0].name, "tokyonight", "default first");
        assert!(choices.len() >= BUILTIN_THEMES.len());
    }

    #[test]
    fn filter_themes_matches_name_and_source() {
        let choices: Vec<ThemeChoice> = theme_choices()
            .into_iter()
            .filter(|c| c.source == "built-in")
            .collect();
        assert_eq!(filter_themes(&choices, "").len(), choices.len());
        let nord = filter_themes(&choices, "nord");
        assert_eq!(nord.len(), 1);
        assert_eq!(choices[nord[0]].name, "nord");
        // Source column matches too.
        assert_eq!(filter_themes(&choices, "built-in").len(), choices.len());
        assert!(filter_themes(&choices, "zzz").is_empty());
    }
}
