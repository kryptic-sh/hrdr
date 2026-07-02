//! Frontend/display configuration shared by hrdr's frontends: [`UiConfig`]
//! (the knobs that affect how a frontend renders and behaves, split out of
//! `hrdr_agent::AgentConfig` so the core agent crate stays
//! representation-agnostic) plus the config-string ⇄ enum mappings for the
//! display modes, so the TUI and GUI resolve and persist settings identically.

use hrdr_agent::parse_env_bool;

/// Default lifetime (in turns) a completed TODO item stays visible before it's
/// pruned: the turn it finishes plus four more.
pub const DEFAULT_TODO_TTL: u64 = 5;

/// Frontend/display configuration. Loaded from the same
/// `~/.config/hrdr/config.toml` + `HRDR_*` env vars as
/// [`hrdr_agent::AgentConfig`] (precedence: env > file > default) — the file
/// keys and env names are unchanged; only the owning crate moved.
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// Input discipline for the TUI: `true` = vim (hjkl), `false` = plain
    /// claude-style input (default). CLI `--vim`.
    pub vim_mode: bool,
    /// Path to an hjkl theme TOML; `None` uses the bundled default.
    pub theme: Option<String>,
    /// Icon set for the TUI: `nerd` (default), `unicode`, or `ascii`. `None`
    /// resolves to nerd (there's no portable way to probe the terminal font).
    pub icons: Option<String>,
    /// Per-message timestamp style: `none`, `relative` (default), or `exact`
    /// (see [`TimestampStyle`]).
    pub timestamps: Option<String>,
    /// Status-bar mode: `none`, `truncate` (default), or `wrap` (see
    /// [`StatusBarMode`]).
    pub statusbar: Option<String>,
    /// Ring the terminal bell when a turn finishes (after a short minimum
    /// duration, so quick turns stay quiet). Default `true`.
    pub bell: bool,
    /// On TUI startup, resume the most recent session for the cwd. Default
    /// `true`.
    pub auto_resume: bool,
    /// How many turns a completed TODO item stays visible before it's pruned.
    /// Default [`DEFAULT_TODO_TTL`].
    pub todo_ttl: u64,
    /// Show the model's `<think>` reasoning blocks. Default `true`. Toggled at
    /// runtime by `/thinking` (aka `/reasoning`).
    pub show_thinking: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            vim_mode: false,
            theme: None,
            icons: None,
            timestamps: None,
            statusbar: None,
            bell: true,
            auto_resume: true,
            todo_ttl: DEFAULT_TODO_TTL,
            show_thinking: true,
        }
    }
}

/// Subset of config.toml the UI layer parses; all fields optional. Unknown
/// keys (the agent's) are ignored, and vice versa — both layers read the same
/// file leniently.
#[derive(serde::Deserialize, Default)]
struct UiFileConfig {
    vim: Option<bool>,
    theme: Option<String>,
    icons: Option<String>,
    timestamps: Option<String>,
    statusbar: Option<String>,
    bell: Option<bool>,
    auto_resume: Option<bool>,
    todo_ttl: Option<u64>,
    show_thinking: Option<bool>,
}

impl UiConfig {
    /// Load with precedence env > config file > defaults. Lenient like
    /// [`hrdr_agent::AgentConfig::load`]: a malformed file is treated as
    /// absent (the agent-side `load_checked` already surfaces the warning).
    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Some(path) = hrdr_agent::config_file_path()
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(fc) = toml::from_str::<UiFileConfig>(&text)
        {
            cfg.apply_file(fc);
        }
        cfg.apply_env();
        cfg
    }

    fn apply_file(&mut self, fc: UiFileConfig) {
        if let Some(v) = fc.vim {
            self.vim_mode = v;
        }
        if let Some(v) = fc.theme {
            self.theme = Some(v);
        }
        if let Some(v) = fc.icons {
            self.icons = Some(v);
        }
        if let Some(v) = fc.timestamps {
            self.timestamps = Some(v);
        }
        if let Some(v) = fc.statusbar {
            self.statusbar = Some(v);
        }
        if let Some(v) = fc.bell {
            self.bell = v;
        }
        if let Some(v) = fc.auto_resume {
            self.auto_resume = v;
        }
        if let Some(v) = fc.todo_ttl {
            self.todo_ttl = v;
        }
        if let Some(v) = fc.show_thinking {
            self.show_thinking = v;
        }
    }

    fn apply_env(&mut self) {
        for (name, set) in UI_ENV_SETTERS {
            if let Ok(v) = std::env::var(name) {
                set(self, v);
            }
        }
    }
}

/// Env var → setter table for [`UiConfig::apply_env`]; one row per knob, same
/// var names as before the AgentConfig split.
type UiEnvSetter = fn(&mut UiConfig, String);
const UI_ENV_SETTERS: &[(&str, UiEnvSetter)] = &[
    ("HRDR_THEME", |c, v| c.theme = Some(v)),
    ("HRDR_ICONS", |c, v| c.icons = Some(v)),
    ("HRDR_TIMESTAMPS", |c, v| c.timestamps = Some(v)),
    ("HRDR_STATUSBAR", |c, v| c.statusbar = Some(v)),
    ("HRDR_BELL", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.bell = b;
        }
    }),
    ("HRDR_AUTO_RESUME", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.auto_resume = b;
        }
    }),
    ("HRDR_TODO_TTL", |c, v| {
        if let Ok(n) = v.parse() {
            c.todo_ttl = n;
        }
    }),
    ("HRDR_SHOW_THINKING", |c, v| {
        if let Some(b) = parse_env_bool(&v) {
            c.show_thinking = b;
        }
    }),
];

/// Per-message timestamp display style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimestampStyle {
    /// No timestamps/numbers.
    None,
    /// Relative (`now`, `2m ago`, `3h ago`).
    Relative,
    /// Exact local time (`HH:MM`).
    Exact,
}

impl TimestampStyle {
    /// Resolve from a config string; anything unrecognized (incl. `None`) is
    /// `Relative` — the default.
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|x| x.trim().to_ascii_lowercase()).as_deref() {
            Some("none" | "off" | "hidden" | "false" | "0") => Self::None,
            Some("exact" | "absolute" | "abs") => Self::Exact,
            _ => Self::Relative,
        }
    }

    /// Canonical config string, for persistence (round-trips `from_config`).
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Relative => "relative",
            Self::Exact => "exact",
        }
    }
}

/// How the status bar behaves when it doesn't fit the terminal width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusBarMode {
    /// Hidden entirely.
    None,
    /// Drop the least-important sections until it fits one row.
    Truncate,
    /// Wrap onto multiple rows so everything is shown.
    Wrap,
}

impl StatusBarMode {
    /// Resolve from a config string; anything unrecognized (incl. `None`) is
    /// `Truncate` — the default.
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|x| x.trim().to_ascii_lowercase()).as_deref() {
            Some("none" | "off" | "hidden") => Self::None,
            Some("wrap") => Self::Wrap,
            _ => Self::Truncate,
        }
    }

    /// Canonical config string, for persistence (round-trips `from_config`).
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Truncate => "truncate",
            Self::Wrap => "wrap",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_file_config_applies_over_defaults() {
        let fc: UiFileConfig = toml::from_str(
            r#"
            vim = true
            theme = "dark"
            icons = "ascii"
            timestamps = "exact"
            statusbar = "wrap"
            bell = false
            auto_resume = false
            todo_ttl = 10
            show_thinking = false
            # agent-side keys are ignored, not an error:
            model = "qwen3"
            temperature = 0.5
            "#,
        )
        .unwrap();
        let mut cfg = UiConfig::default();
        cfg.apply_file(fc);
        assert!(cfg.vim_mode);
        assert_eq!(cfg.theme.as_deref(), Some("dark"));
        assert_eq!(cfg.icons.as_deref(), Some("ascii"));
        assert_eq!(cfg.timestamps.as_deref(), Some("exact"));
        assert_eq!(cfg.statusbar.as_deref(), Some("wrap"));
        assert!(!cfg.bell);
        assert!(!cfg.auto_resume);
        assert_eq!(cfg.todo_ttl, 10);
        assert!(!cfg.show_thinking);
        // Empty file keeps defaults.
        let mut d = UiConfig::default();
        d.apply_file(UiFileConfig::default());
        assert!(!d.vim_mode);
        assert!(d.bell && d.auto_resume && d.show_thinking);
        assert_eq!(d.todo_ttl, DEFAULT_TODO_TTL);
    }

    #[test]
    fn timestamp_style_from_config() {
        assert_eq!(
            TimestampStyle::from_config(Some("off")),
            TimestampStyle::None
        );
        assert_eq!(
            TimestampStyle::from_config(Some("ABS")),
            TimestampStyle::Exact
        );
        assert_eq!(TimestampStyle::from_config(None), TimestampStyle::Relative);
        assert_eq!(
            TimestampStyle::from_config(Some("garbage")),
            TimestampStyle::Relative
        );
    }

    #[test]
    fn status_bar_mode_from_config() {
        assert_eq!(
            StatusBarMode::from_config(Some("hidden")),
            StatusBarMode::None
        );
        assert_eq!(
            StatusBarMode::from_config(Some("wrap")),
            StatusBarMode::Wrap
        );
        assert_eq!(StatusBarMode::from_config(None), StatusBarMode::Truncate);
    }

    #[test]
    fn config_strings_round_trip() {
        for s in [
            TimestampStyle::None,
            TimestampStyle::Relative,
            TimestampStyle::Exact,
        ] {
            assert_eq!(TimestampStyle::from_config(Some(s.as_config_str())), s);
        }
        for m in [
            StatusBarMode::None,
            StatusBarMode::Truncate,
            StatusBarMode::Wrap,
        ] {
            assert_eq!(StatusBarMode::from_config(Some(m.as_config_str())), m);
        }
    }
}
