//! Representation-independent config-value enums shared by hrdr's frontends:
//! parsing a config/CLI string into a display mode, and back to its canonical
//! config string for persistence. The rendering each mode implies is the
//! frontend's business; the string ⇄ enum mapping is shared so the TUI and GUI
//! resolve and persist settings identically.

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
