//! Chat-UI color theme, resolved from an [`hjkl_theme::Theme`].
//!
//! hrdr reuses hjkl's theme system: a theme is an hjkl theme TOML (palette +
//! `[ui]` styles), loaded via `hjkl_theme`, and converted to ratatui colors
//! with `hjkl_theme_tui::ToRatatui`. We map hjkl's editor-oriented palette onto
//! hrdr's chat roles, with sensible fallbacks for anything a theme omits.

use std::path::Path;

use hjkl_markdown_tui::MdTheme;
use hjkl_theme::Theme as HjklTheme;
use hjkl_theme::loader;
use hjkl_theme_tui::ToRatatui;
use ratatui::style::Color;

/// Resolved colors for hrdr's chat surfaces.
#[derive(Debug, Clone)]
pub struct Theme {
    /// User prompt accent (the `❯` and user text).
    pub user: Color,
    /// Assistant message text.
    pub assistant: Color,
    /// Dimmed chrome: reasoning, system lines, stats, borders, hints, scrollbar.
    pub dim: Color,
    /// Attention color: tool names, the inference loader, the follow button.
    pub warn: Color,
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
    /// Load a theme from `path` (an hjkl theme TOML), falling back to hjkl's
    /// bundled default if the path is `None` or fails to parse.
    pub fn load(path: Option<&str>) -> Self {
        let hjkl = match path {
            Some(p) => {
                HjklTheme::from_path(Path::new(p)).unwrap_or_else(|_| loader::default_theme())
            }
            None => loader::default_theme(),
        };
        Self::from_hjkl(&hjkl)
    }

    /// Map an hjkl theme's palette + UI styles onto hrdr's chat roles.
    pub fn from_hjkl(t: &HjklTheme) -> Self {
        let pal = |name: &str| t.palette.get(name).map(|c| c.to_ratatui());
        let ui_fg = t.ui.foreground.map(|c| c.to_ratatui());
        let ui_gutter = t.ui.gutter.map(|c| c.to_ratatui());

        Self {
            user: pal("teal").or_else(|| pal("blue")).unwrap_or(Color::Cyan),
            assistant: ui_fg.or_else(|| pal("fg")).unwrap_or(Color::White),
            dim: ui_gutter
                .or_else(|| pal("comment"))
                .unwrap_or(Color::DarkGray),
            warn: pal("yellow").unwrap_or(Color::Yellow),
            success: pal("green").unwrap_or(Color::Green),
            error: t
                .ui
                .diagnostic_error
                .map(|c| c.to_ratatui())
                .or_else(|| pal("red"))
                .unwrap_or(Color::Red),
            accent: pal("blue").or_else(|| pal("teal")).unwrap_or(Color::Blue),
            accent2: pal("magenta")
                .or_else(|| pal("purple"))
                .or_else(|| pal("blue"))
                .unwrap_or(Color::Magenta),
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
}

impl Default for Theme {
    fn default() -> Self {
        Self::load(None)
    }
}
