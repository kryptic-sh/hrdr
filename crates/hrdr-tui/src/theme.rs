//! Chat-UI color theme for the terminal.
//!
//! hrdr reuses hjkl's theme system (a theme TOML with a palette + `[ui]`
//! styles). The role mapping — which palette entries feed which chat role —
//! is shared via [`hrdr_app::ChatPalette`]; this module only
//! converts the resolved RGB roles to ratatui colors.

use hjkl_markdown_tui::MdTheme;
use hjkl_theme::Theme as HjklTheme;
use hrdr_app::ChatPalette;
use ratatui::style::Color;

/// The default theme, baked into the binary at compile time.
const DEFAULT_THEME_TOML: &str = include_str!("theme.toml");

/// Resolved colors for hrdr's chat surfaces.
#[derive(Debug, Clone)]
pub struct Theme {
    /// User prompt accent (the `❯` and user text).
    pub user: Color,
    /// User prompt background.
    pub user_bg: Color,
    /// Assistant message text.
    pub assistant: Color,
    /// Dimmed chrome: reasoning, system lines, stats, borders, hints, scrollbar.
    pub dim: Color,
    /// Attention color: tool names, the inference loader, the follow button.
    pub warn: Color,
    /// Slash-command output block background.
    pub command_bg: Color,
    /// Per-turn stats block background.
    pub stats_bg: Color,
    /// Session-header (banner) block background.
    pub header_bg: Color,
    /// Success marks (tool ✓).
    pub success: Color,
    /// Error marks (tool ✗) and the quit-confirm banner.
    pub error: Color,
    /// Secondary accent (blue) — extra variety for status-bar sections.
    pub accent: Color,
    /// Tertiary accent (magenta/purple) — extra variety for status-bar sections.
    pub accent2: Color,
}

impl Theme {
    /// Load a theme from `path` (an hjkl theme TOML). When `path` is `None` or
    /// fails to parse, falls back to the baked-in default theme.
    pub fn load(path: Option<&str>) -> Self {
        let hjkl = match path {
            Some(p) => HjklTheme::from_path(std::path::Path::new(p))
                .unwrap_or_else(|_| default_hjkl_theme()),
            None => default_hjkl_theme(),
        };
        Self::from_palette(&ChatPalette::from_hjkl(&hjkl))
    }

    /// Apply terminal fallback colors to any palette role that the theme
    /// omitted. The baked-in default theme defines every role, so these
    /// only fire for incomplete user themes.
    fn from_palette(p: &ChatPalette) -> Self {
        let c = |rgb: Option<(u8, u8, u8)>, fb: Color| {
            rgb.map(|(r, g, b)| Color::Rgb(r, g, b)).unwrap_or(fb)
        };
        Self {
            user: c(p.user, Color::Cyan),
            user_bg: c(p.user_bg, Color::Rgb(0, 48, 60)),
            assistant: c(p.assistant, Color::White),
            dim: c(p.dim, Color::DarkGray),
            warn: c(p.warn, Color::Yellow),
            command_bg: c(p.command_bg, Color::Rgb(32, 34, 58)),
            stats_bg: c(p.stats_bg, Color::Rgb(25, 27, 43)),
            header_bg: c(p.header_bg, Color::Rgb(22, 24, 42)),
            success: c(p.success, Color::Green),
            error: c(p.error, Color::Red),
            accent: c(p.accent, Color::Blue),
            accent2: c(p.accent2, Color::Magenta),
        }
    }

    /// Markdown render theme derived from these chat colors, so assistant
    /// markdown follows the active hjkl theme.
    pub fn md_theme(&self) -> MdTheme {
        MdTheme::new(
            self.assistant, // text
            self.user,      // heading1
            self.warn,      // heading 2-6
            self.success,   // inline code span
            self.success,   // code block
            self.user,      // link
            self.warn,      // list bullet
            self.assistant, // bold
            self.assistant, // italic
            self.dim,       // rule
        )
    }

    /// [`Self::md_theme`] with every role dimmed: reasoning renders with the
    /// same structure and colors as output, only quieter.
    pub fn md_theme_dim(&self) -> MdTheme {
        let d = |c: Color| dim_color(c, REASONING_DIM);
        MdTheme::new(
            d(self.assistant),
            d(self.user),
            d(self.warn),
            d(self.success),
            d(self.success),
            d(self.user),
            d(self.warn),
            d(self.assistant),
            d(self.assistant),
            d(self.dim),
        )
    }
}

/// How much of a color's brightness reasoning text keeps.
const REASONING_DIM: f32 = 0.55;

/// Scale an RGB color's brightness by `factor`. Named/indexed terminal colors
/// have no components to scale, so they pass through unchanged.
fn dim_color(c: Color, factor: f32) -> Color {
    let s = |v: u8| (v as f32 * factor).round().clamp(0.0, 255.0) as u8;
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(s(r), s(g), s(b)),
        other => other,
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::load(None)
    }
}

/// Parse the baked-in default theme TOML.
fn default_hjkl_theme() -> HjklTheme {
    HjklTheme::from_toml_str(DEFAULT_THEME_TOML)
        .unwrap_or_else(|_| hjkl_theme::loader::default_theme())
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    fn hex(c: Color) -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            other => panic!("expected an RGB color, got {other:?}"),
        }
    }

    /// The bundled theme must *parse*. `Theme::load` swallows a parse error and
    /// silently falls back to hjkl's bundled default (a different palette
    /// entirely), so a typo here would ship the wrong colors with no warning.
    ///
    /// Regression: `[palette]` values must be literal hex — only `[ui]` resolves
    /// `$refs` — and writing `bg_user = "$bg_visual"` broke the whole file.
    #[test]
    fn the_bundled_theme_parses() {
        hjkl_theme::Theme::from_toml_str(DEFAULT_THEME_TOML)
            .expect("the bundled theme must parse, or Theme::load silently uses another palette");
    }

    /// Every chat role resolves to its Tokyo Night (night) value — i.e. the
    /// bundled theme really is the theme in force, not a fallback.
    #[test]
    fn every_role_resolves_to_a_tokyo_night_color() {
        let t = Theme::default();
        assert_eq!(hex(t.user), "#7dcfff", "cyan");
        assert_eq!(hex(t.assistant), "#c0caf5", "fg");
        assert_eq!(hex(t.dim), "#565f89", "comment");
        assert_eq!(hex(t.warn), "#e0af68", "yellow");
        assert_eq!(hex(t.success), "#9ece6a", "green");
        assert_eq!(hex(t.error), "#f7768e", "red");
        assert_eq!(hex(t.accent), "#7aa2f7", "blue");
        assert_eq!(hex(t.accent2), "#bb9af7", "magenta");

        assert_eq!(hex(t.user_bg), "#1e2030", "bg_dark (moon)");
        assert_eq!(hex(t.command_bg), "#24283b", "bg_storm");
        assert_eq!(hex(t.stats_bg), "#222436", "bg_moon");
        assert_eq!(hex(t.header_bg), "#292e42", "bg_highlight");
    }

    /// The two accents must differ.
    ///
    /// Regression: the theme named the purple `mauve` (a Catppuccin name) while
    /// `ChatPalette` looks up `magenta` (Tokyo Night's). The lookup missed and
    /// `accent2` silently fell through to `blue` — the same color as `accent`.
    #[test]
    fn the_two_accents_are_distinct() {
        let t = Theme::default();
        assert_ne!(t.accent, t.accent2, "accent2 fell back to accent");
    }
}
