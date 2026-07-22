//! Pane switcher view-model: the render-facing rows a frontend draws for the
//! agent list.
//!
//! The panes themselves — the manage-a-set-of-agent-conversations core — live in
//! [`hrdr_agent`] ([`Pane`], [`PaneSet`], [`PaneStatus`]). This module turns that
//! set into a flat list of rows for the TUI's pane switcher, and picks the marker
//! a row shows given a frontend-supplied running spinner.

use crate::{PaneId, PaneSet, PaneStatus};

/// One row of the pane switcher: the main agent first, then each sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub id: PaneId,
    pub title: String,
    pub status: PaneStatus,
    /// The row is the pane currently being displayed.
    pub active: bool,
}

/// The switcher's rows. Main is always first — it is how you get back.
pub fn pane_rows(panes: &PaneSet) -> Vec<PaneRow> {
    std::iter::once(panes.main())
        .chain(panes.subs())
        .map(|p| PaneRow {
            id: p.id,
            title: p.title().to_string(),
            status: p.status,
            active: panes.active() == p.id,
        })
        .collect()
}

/// The marker a row shows: running spinner, finished tick, or idle dot.
pub fn pane_row_marker(status: PaneStatus, running_marker: &str) -> String {
    match status {
        PaneStatus::Running => running_marker.to_string(),
        PaneStatus::Done => "✓".to_string(),
        PaneStatus::Idle => "·".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_is_always_the_first_row_so_you_can_get_back() {
        let mut panes = PaneSet::new();
        let rows = pane_rows(&panes);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, PaneId::Main);
        assert!(rows[0].active, "main is active by default");
        assert_eq!(rows[0].title, "main");

        // The row names the *agent*, not the session — naming it after the session
        // (which the status bar already shows) says nothing about which agent it is.
        panes.main_mut().state.name = "my session".to_string();
        assert_eq!(pane_rows(&panes)[0].title, "main");
    }
}
