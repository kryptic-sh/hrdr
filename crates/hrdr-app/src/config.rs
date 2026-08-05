//! Frontend/display configuration shared by hrdr's frontends: [`UiConfig`]
//! (the knobs that affect how a frontend renders and behaves, split out of
//! `hrdr_agent::AgentConfig` so the core agent crate stays
//! representation-agnostic) plus the config-string ⇄ enum mappings for the
//! display modes, so every frontend resolves and persists settings identically.
//!
//! # Validation policy
//!
//! The enum-like display settings (`icons`, `statusbar`) are
//! validated by [`UiConfig::validate`]: an unrecognized value produces a
//! **warning** that names the valid options, rather than silently falling back
//! to the default. These are cosmetic per-frontend preferences — a typo should
//! not refuse to start the whole app — so they warn and default, unlike the
//! agent-side config-file values that are hard errors (see
//! `hrdr_agent::config`). Because a bad value defaults the same way whether it
//! came from the file or a `HRDR_*` env var, [`UiConfig::validate`] checks the
//! resolved value and does not distinguish the two sources.

use hrdr_agent::parse_env_bool;

/// Default lifetime (in turns) a completed TODO item stays visible before it's
/// pruned: the turn it finishes plus four more.
pub const DEFAULT_TODO_TTL_TURNS: u64 = 5;

/// Max text rows the input box auto-grows to.
pub const INPUT_MAX_ROWS: u16 = 5;

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
    /// Default [`DEFAULT_TODO_TTL_TURNS`].
    pub todo_ttl: u64,
    /// Max transcript entries kept in the scrollback buffer. Older entries are
    /// evicted from the front to keep render performance stable. Default 500.
    pub scrollback: usize,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            vim_mode: false,
            theme: None,
            icons: None,
            statusbar: None,
            bell: true,
            auto_resume: true,
            todo_ttl: DEFAULT_TODO_TTL_TURNS,
            scrollback: 500,
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
    statusbar: Option<String>,
    bell: Option<bool>,
    auto_resume: Option<bool>,
    todo_ttl: Option<u64>,
    scrollback: Option<usize>,
}

impl UiConfig {
    /// Load with precedence env > config file > defaults. Lenient like
    /// [`hrdr_agent::AgentConfig::load`]: a malformed file is treated as
    /// absent (the agent-side `load_checked` already surfaces the warning).
    pub fn load() -> Self {
        Self::load_diagnosed().0
    }

    /// Load, returning the config alongside any warnings about unrecognized
    /// enum-like settings (see [`validate`](Self::validate)). The frontends
    /// surface the warnings as a startup notice; [`load`](Self::load) drops them.
    pub fn load_diagnosed() -> (Self, Vec<String>) {
        let mut cfg = Self::default();
        if let Some(fc) = hrdr_agent::read_config_file::<UiFileConfig>() {
            cfg.apply_file(fc);
        }
        cfg.apply_env();
        let warnings = cfg.validate();
        (cfg, warnings)
    }

    /// Warn about unrecognized enum-like settings, naming the valid options for
    /// each (`icons`, `statusbar`). A warning, not an error: the
    /// setting falls back to its default (see the [module docs](self)). Checks
    /// the resolved value, so it covers both the config file and `HRDR_*` env.
    pub fn validate(&self) -> Vec<String> {
        /// Recognized spellings for one setting, and the canonical options to
        /// print. `recognized` includes aliases the `from_config` mappings
        /// accept; `options` is the short human-facing list.
        fn check(
            warnings: &mut Vec<String>,
            field: &str,
            value: Option<&str>,
            recognized: &[&str],
            options: &str,
        ) {
            if let Some(raw) = value {
                let v = raw.trim().to_ascii_lowercase();
                if !recognized.contains(&v.as_str()) {
                    warnings.push(format!(
                        "{field} = \"{raw}\" is not a known value (valid: {options}); \
                         using the default"
                    ));
                }
            }
        }
        let mut warnings = Vec::new();
        check(
            &mut warnings,
            "icons",
            self.icons.as_deref(),
            &["nerd", "unicode", "ascii"],
            "nerd, unicode, ascii",
        );
        check(
            &mut warnings,
            "statusbar",
            self.statusbar.as_deref(),
            &["none", "off", "hidden", "truncate", "wrap"],
            "none, truncate, wrap",
        );
        warnings
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
        if let Some(v) = fc.scrollback {
            self.scrollback = v;
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
    ("HRDR_SCROLLBACK", |c, v| {
        if let Ok(n) = v.parse() {
            c.scrollback = n;
        }
    }),
];

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
            statusbar = "wrap"
            bell = false
            auto_resume = false
            todo_ttl = 10
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
        assert_eq!(cfg.statusbar.as_deref(), Some("wrap"));
        assert!(!cfg.bell);
        assert!(!cfg.auto_resume);
        assert_eq!(cfg.todo_ttl, 10);
        // Empty file keeps defaults.
        let mut d = UiConfig::default();
        d.apply_file(UiFileConfig::default());
        assert!(!d.vim_mode);
        assert!(d.bell && d.auto_resume);
        assert_eq!(d.todo_ttl, DEFAULT_TODO_TTL_TURNS);
    }

    /// ONE `config.toml`, TWO readers — and the agent's reader now refuses
    /// unknown keys, so it has to know the frontend's.
    ///
    /// The regression this exists for: `deny_unknown_fields` went onto the
    /// agent's `FileConfig` to make a typo fail loudly, and made every *display*
    /// setting fail loudly too. A config that had worked for months —
    /// `statusbar = "truncate"`, `theme = "tokyonight"` — refused to start, and
    /// the error helpfully listed the valid keys, none of which was the one the
    /// user had written. The two layers cannot see each other's structs (the
    /// dependency runs one way), so this walks every key the UI parses through
    /// the agent's loader and fails if any is rejected.
    #[test]
    fn the_agent_accepts_every_ui_key() {
        // Every field of `UiFileConfig`, with a value of the right shape. Adding a
        // UI key without declaring it on the agent's `FileConfig` fails here.
        const UI_KEYS: &str = r#"
            vim = true
            theme = "tokyonight"
            icons = "nerd"
            statusbar = "truncate"
            bell = true
            auto_resume = true
            todo_ttl = 10
            scrollback = 500
        "#;
        // The UI reads them all…
        let ui: UiFileConfig = toml::from_str(UI_KEYS).expect("the UI parses its own keys");
        let mut cfg = UiConfig::default();
        cfg.apply_file(ui);
        assert_eq!(cfg.statusbar.as_deref(), Some("truncate"));

        // …and the AGENT accepts the same file rather than refusing to start.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("{UI_KEYS}\nmodel = \"zen://kimi-k2\"\n"))
            .expect("the config is written");
        let errors = hrdr_agent::config_file_errors(&path);
        assert!(
            errors.is_empty(),
            "the agent refused the frontend's own keys: {errors:?}"
        );

        // …while a key NEITHER layer knows is still refused — the whole point of
        // `deny_unknown_fields` survives.
        std::fs::write(&path, "tiemstamps = \"relative\"\n").expect("written");
        assert!(
            !hrdr_agent::config_file_errors(&path).is_empty(),
            "a typo must still fail loudly"
        );
    }

    #[test]
    fn unknown_enum_values_warn_naming_valid_options() {
        let mut cfg = UiConfig {
            icons: Some("nerdfont".to_string()),
            statusbar: Some("compact".to_string()),
            ..Default::default()
        };
        let warnings = cfg.validate();
        // Every bad enum is reported together, each naming its valid options.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("icons") && w.contains("nerd, unicode, ascii")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("statusbar") && w.contains("none, truncate, wrap")),
            "{warnings:?}"
        );

        // Recognized values (including aliases and case/whitespace) warn about
        // nothing; unset fields are silent.
        cfg.icons = Some("ASCII".to_string());
        cfg.statusbar = None;
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
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
        for m in [
            StatusBarMode::None,
            StatusBarMode::Truncate,
            StatusBarMode::Wrap,
        ] {
            assert_eq!(StatusBarMode::from_config(Some(m.as_config_str())), m);
        }
    }
}
