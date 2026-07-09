//! Chat-role color resolution from an hjkl theme, shared by the frontends.
//!
//! hrdr reuses hjkl's theme system (a palette + `[ui]` styles TOML). The
//! mapping from that editor-oriented palette onto hrdr's *chat roles* — which
//! palette entries feed which role, and in what fallback order — lives here so
//! the TUI and GUI can't drift. Each role resolves to an RGB triple or `None`
//! (the theme doesn't cover it); the frontends convert to their own color type
//! and apply their own medium-appropriate final fallbacks (ANSI names in the
//! terminal, RGB constants in the GUI).

use hjkl_theme::Theme as HjklTheme;
use hjkl_theme::loader;

/// An RGB triple resolved from a theme.
pub type Rgb = (u8, u8, u8);

/// hrdr's chat-role colors as resolved from an hjkl theme (before frontend
/// fallbacks).
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatPalette {
    /// Window/terminal background.
    pub background: Option<Rgb>,
    /// User prompt accent (the `❯` and user text).
    pub user: Option<Rgb>,
    /// User prompt background.
    pub user_bg: Option<Rgb>,
    /// Assistant message text.
    pub assistant: Option<Rgb>,
    /// Assistant (and reasoning) block background.
    pub assistant_bg: Option<Rgb>,
    /// Dimmed chrome: reasoning, system lines, stats, borders, hints.
    pub dim: Option<Rgb>,
    /// Attention color: tool names, the inference loader.
    pub warn: Option<Rgb>,
    /// Tool-call block background.
    pub tool_bg: Option<Rgb>,
    /// Slash-command output block background.
    pub command_bg: Option<Rgb>,
    /// Per-turn stats block background.
    pub stats_bg: Option<Rgb>,
    /// Session-header (banner) block background.
    pub header_bg: Option<Rgb>,
    /// Success marks (tool ✓).
    pub success: Option<Rgb>,
    /// Error marks (tool ✗).
    pub error: Option<Rgb>,
    /// Secondary accent (blue).
    pub accent: Option<Rgb>,
    /// Tertiary accent (magenta/purple).
    pub accent2: Option<Rgb>,
}

/// Load an hjkl theme TOML from `path`, falling back to hjkl's bundled default
/// when `path` is `None` or fails to parse (both frontends' load policy).
pub fn load_hjkl_theme(path: Option<&str>) -> HjklTheme {
    match path {
        Some(p) => HjklTheme::from_path(std::path::Path::new(p))
            .unwrap_or_else(|_| loader::default_theme()),
        None => loader::default_theme(),
    }
}

impl ChatPalette {
    /// [`load_hjkl_theme`] + [`ChatPalette::from_hjkl`].
    pub fn load(path: Option<&str>) -> Self {
        Self::from_hjkl(&load_hjkl_theme(path))
    }

    /// Map an hjkl theme's palette + UI styles onto hrdr's chat roles.
    pub fn from_hjkl(t: &HjklTheme) -> Self {
        let rgb = |c: hjkl_theme::Color| (c.r, c.g, c.b);
        let pal = |name: &str| t.palette.get(name).copied().map(rgb);
        // The syntect code-block background is the natural tool bg fallback.
        let panel = crate::panel_bg_rgb();
        Self {
            background: t.ui.background.map(rgb),
            user: pal("teal").or_else(|| pal("blue")),
            user_bg: pal("bg_user")
                .or_else(|| pal("ui_selection"))
                .or(Some((0, 48, 60))),
            assistant: t.ui.foreground.map(rgb).or_else(|| pal("fg")),
            assistant_bg: pal("bg_assistant").or_else(|| pal("ui_cursorline")),
            dim: t.ui.gutter.map(rgb).or_else(|| pal("comment")),
            warn: pal("yellow"),
            tool_bg: pal("bg_tool")
                .or_else(|| pal("ui_cursorline"))
                .or(Some(panel)),
            command_bg: pal("bg_command")
                .or_else(|| pal("ui_cursorline"))
                .or(Some(panel)),
            stats_bg: pal("bg_stats").or_else(|| pal("ui_cursorline")),
            header_bg: pal("bg_header").or_else(|| pal("ui_cursorline")),
            success: pal("green"),
            error: t.ui.diagnostic_error.map(rgb).or_else(|| pal("red")),
            accent: pal("blue").or_else(|| pal("teal")),
            accent2: pal("magenta")
                .or_else(|| pal("purple"))
                .or_else(|| pal("blue")),
        }
    }
}
