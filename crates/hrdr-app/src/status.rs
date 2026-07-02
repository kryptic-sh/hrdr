//! Shared status-bar content: which sections exist, their text, their drop
//! priority, and their color roles — identical in the TUI and GUI so the two
//! bars can't drift. Layout is the frontend's job: the TUI fits sections to
//! the terminal width (truncate/wrap per [`crate::StatusBarMode`]), the GUI
//! lays them out with flexbox; both map [`StatusRole`]s onto their theme.

use crate::fmt_count;

/// Semantic color role of a status run — frontends map these to theme colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRole {
    /// Working-directory name (user accent).
    Dir,
    /// Git branch (success green).
    Branch,
    /// Session input tokens (accent).
    TokensIn,
    /// Session output tokens (secondary accent).
    TokensOut,
    /// The filled part of the context gauge (inverted text on the level color).
    CtxFill(CtxLevel),
    /// The unfilled part of the context gauge (text on a dim background).
    CtxRest,
    /// Plain context count when the window size is unknown (warn).
    CtxPlain,
    /// Model name (default foreground).
    Model,
    /// Reasoning-effort label (warn).
    Effort,
    /// Time-to-first-token of the latest turn (dim).
    Ttft,
}

/// Theme slot a semantic role colors from. Both frontends' themes expose
/// these eight colors (under their own field names); mapping slot → concrete
/// color is the only per-frontend piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSlot {
    User,
    Assistant,
    Dim,
    Warn,
    Success,
    Error,
    Accent,
    Accent2,
}

/// Renderer-agnostic style spec: which theme slots to paint with. `fg: None`
/// means inverted text (black) over `bg` — used by the context-gauge fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleStyle {
    pub fg: Option<ThemeSlot>,
    pub bg: Option<ThemeSlot>,
    pub bold: bool,
}

impl RoleStyle {
    const fn fg(slot: ThemeSlot) -> Self {
        Self {
            fg: Some(slot),
            bg: None,
            bold: false,
        }
    }
}

/// The one place the status-bar color semantics live — frontends resolve the
/// returned slots against their theme instead of re-deciding role → color.
pub fn status_role_style(role: StatusRole) -> RoleStyle {
    match role {
        StatusRole::Dir => RoleStyle::fg(ThemeSlot::User),
        StatusRole::Branch => RoleStyle::fg(ThemeSlot::Success),
        StatusRole::TokensIn => RoleStyle::fg(ThemeSlot::Accent),
        StatusRole::TokensOut => RoleStyle::fg(ThemeSlot::Accent2),
        StatusRole::CtxFill(level) => RoleStyle {
            fg: None,
            bg: Some(ctx_level_slot(level)),
            bold: true,
        },
        StatusRole::CtxRest => RoleStyle {
            fg: Some(ThemeSlot::Assistant),
            bg: Some(ThemeSlot::Dim),
            bold: false,
        },
        StatusRole::CtxPlain => RoleStyle::fg(ThemeSlot::Warn),
        StatusRole::Model => RoleStyle::fg(ThemeSlot::Assistant),
        StatusRole::Effort => RoleStyle::fg(ThemeSlot::Warn),
        StatusRole::Ttft => RoleStyle::fg(ThemeSlot::Dim),
    }
}

/// The gauge-fill color slot for a context-usage level (also used by the
/// GUI's real progress-bar gauge).
pub fn ctx_level_slot(level: CtxLevel) -> ThemeSlot {
    match level {
        CtxLevel::Ok => ThemeSlot::Success,
        CtxLevel::Warn => ThemeSlot::Warn,
        CtxLevel::Critical => ThemeSlot::Error,
    }
}

/// Diff-line coloring semantics (additions green, deletions red, hunk headers
/// in the user accent, file headers/context dim) — shared with
/// [`crate::classify_diff_line`].
pub fn diff_kind_slot(kind: crate::DiffLineKind) -> ThemeSlot {
    match kind {
        crate::DiffLineKind::Hunk => ThemeSlot::User,
        crate::DiffLineKind::Add => ThemeSlot::Success,
        crate::DiffLineKind::Remove => ThemeSlot::Error,
        crate::DiffLineKind::Meta => ThemeSlot::Dim,
    }
}

/// How full the context window is, for the gauge's fill color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxLevel {
    /// Comfortable (green).
    Ok,
    /// Getting full (amber, ≥70%).
    Warn,
    /// At/over the auto-compact threshold (red).
    Critical,
}

/// One styled run of text within a status section.
#[derive(Debug, Clone)]
pub struct StatusRun {
    pub text: String,
    pub role: StatusRole,
}

/// The context gauge's raw data, for frontends that can draw a real progress
/// bar (the GUI) instead of the character-cell fill the text runs encode.
#[derive(Debug, Clone)]
pub struct CtxGauge {
    /// Fill fraction, `0.0..=1.0`.
    pub frac: f64,
    pub level: CtxLevel,
    /// The gauge's label (`" 12.3k of 32k "`).
    pub label: String,
}

/// One status-bar section: a drop `priority` (higher = dropped first when the
/// TUI truncates) and its styled runs. `gauge` carries the context gauge's
/// raw data when this section is it — text frontends use the pre-split
/// fill/rest runs, pixel frontends draw a real bar from the gauge.
#[derive(Debug, Clone)]
pub struct StatusSeg {
    pub priority: u8,
    pub runs: Vec<StatusRun>,
    pub gauge: Option<CtxGauge>,
}

impl StatusSeg {
    fn one(priority: u8, text: String, role: StatusRole) -> Self {
        Self {
            priority,
            runs: vec![StatusRun { text, role }],
            gauge: None,
        }
    }
    /// Display width in characters.
    pub fn width(&self) -> usize {
        self.runs.iter().map(|r| r.text.chars().count()).sum()
    }
}

/// Everything the status bar shows, gathered by the frontend.
pub struct StatusInputs<'a> {
    /// Display working directory (the basename is shown).
    pub dir: &'a str,
    pub branch: Option<&'a str>,
    /// Session-cumulative prompt/completion tokens.
    pub tokens_in: usize,
    pub tokens_out: usize,
    /// Prompt tokens of the latest model call (context in use).
    pub ctx_used: usize,
    pub context_window: Option<u32>,
    /// Auto-compaction trigger fraction (0 disables the red level).
    pub auto_compact_ratio: f64,
    pub model: &'a str,
    pub effort: Option<&'a str>,
    /// Time-to-first-token of the latest turn, seconds.
    pub ttft: Option<f64>,
    /// Whether Nerd-font glyphs (folder/branch icons) may be used.
    pub nerd_icons: bool,
}

/// Build the status sections in display order. Priorities (drop order under
/// truncation, highest first): ttft 6, effort 5, tokens 4, branch 3, model 2,
/// context 1, dir 0.
pub fn status_sections(i: &StatusInputs) -> Vec<StatusSeg> {
    let mut sections = Vec::new();

    // cwd basename + folder icon (Nerd glyphs only when the icon mode allows).
    let folder = if i.nerd_icons { "\u{f07b} " } else { "" };
    let branch_icon = if i.nerd_icons { "\u{e0a0} " } else { "" };
    let dir_label = i.dir.rsplit('/').find(|s| !s.is_empty()).unwrap_or(i.dir);
    sections.push(StatusSeg::one(
        0,
        format!(" {folder}{dir_label}"),
        StatusRole::Dir,
    ));
    if let Some(branch) = i.branch {
        sections.push(StatusSeg::one(
            3,
            format!("{branch_icon}{branch}"),
            StatusRole::Branch,
        ));
    }
    sections.push(StatusSeg::one(
        4,
        format!("↑{}", fmt_count(i.tokens_in)),
        StatusRole::TokensIn,
    ));
    sections.push(StatusSeg::one(
        4,
        format!("↓{}", fmt_count(i.tokens_out)),
        StatusRole::TokensOut,
    ));
    // Context: a used/free gauge when the window is known, else a plain count.
    match i.context_window {
        Some(w) if w > 0 => {
            let frac = (i.ctx_used as f64 / w as f64).clamp(0.0, 1.0);
            // Fill color escalates with usage: green → amber → red at the
            // auto-compact threshold (where compaction kicks in next turn).
            let level = if i.auto_compact_ratio > 0.0 && frac >= i.auto_compact_ratio {
                CtxLevel::Critical
            } else if frac >= 0.70 {
                CtxLevel::Warn
            } else {
                CtxLevel::Ok
            };
            let label = format!(" {} of {} ", fmt_count(i.ctx_used), fmt_count(w as usize));
            let chars: Vec<char> = label.chars().collect();
            let fill = ((frac * chars.len() as f64).round() as usize).min(chars.len());
            let used: String = chars[..fill].iter().collect();
            let free: String = chars[fill..].iter().collect();
            let mut runs = Vec::new();
            if !used.is_empty() {
                runs.push(StatusRun {
                    text: used,
                    role: StatusRole::CtxFill(level),
                });
            }
            if !free.is_empty() {
                runs.push(StatusRun {
                    text: free,
                    role: StatusRole::CtxRest,
                });
            }
            sections.push(StatusSeg {
                priority: 1,
                runs,
                gauge: Some(CtxGauge { frac, level, label }),
            });
        }
        _ => sections.push(StatusSeg::one(
            1,
            format!(" {} ctx ", fmt_count(i.ctx_used)),
            StatusRole::CtxPlain,
        )),
    }
    sections.push(StatusSeg::one(2, i.model.to_string(), StatusRole::Model));
    if let Some(effort) = i.effort {
        sections.push(StatusSeg::one(5, effort.to_string(), StatusRole::Effort));
    }
    if let Some(secs) = i.ttft {
        sections.push(StatusSeg::one(
            6,
            format!("ttft {secs:.2}s"),
            StatusRole::Ttft,
        ));
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>() -> StatusInputs<'a> {
        StatusInputs {
            dir: "/home/me/proj",
            branch: Some("main"),
            tokens_in: 1200,
            tokens_out: 90,
            ctx_used: 900,
            context_window: Some(1000),
            auto_compact_ratio: 0.85,
            model: "qwen3",
            effort: None,
            ttft: Some(1.5),
            nerd_icons: false,
        }
    }

    #[test]
    fn sections_cover_roles_and_levels() {
        let segs = status_sections(&inputs());
        // dir, branch, in, out, ctx, model, ttft (no effort).
        assert_eq!(segs.len(), 7);
        assert!(segs[0].runs[0].text.contains("proj"));
        // 90% of a 1000-token window with an 0.85 trigger → critical fill.
        let ctx = &segs[4];
        assert_eq!(ctx.runs[0].role, StatusRole::CtxFill(CtxLevel::Critical));
        // The raw gauge data rides along for pixel frontends.
        let gauge = ctx.gauge.as_ref().expect("ctx section carries the gauge");
        assert!((gauge.frac - 0.9).abs() < 1e-9);
        assert_eq!(gauge.level, CtxLevel::Critical);
        assert!(gauge.label.contains("of"));
        // Unknown window → plain count.
        let mut i2 = inputs();
        i2.context_window = None;
        let segs2 = status_sections(&i2);
        assert!(
            segs2
                .iter()
                .any(|s| s.runs.iter().any(|r| r.role == StatusRole::CtxPlain))
        );
    }
}
