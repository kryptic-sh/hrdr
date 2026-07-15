//! Representation-independent sub-agent panel model shared by hrdr's frontends.
//!
//! [`SubAgentPanel`] maintains the live list of running blocking `task`
//! sub-agents, updated via its event-fold methods as `ToolStart`/`ToolOutput`/
//! `ToolEnd` events arrive. [`panel_items`] merges the blocking list with
//! detached background tasks from the shared registry to produce a unified
//! [`Vec<PanelItem>`] ready for rendering by any frontend.
//!
//! The panel is a *list*: one row per agent, no preview of the log and no
//! expansion. The log already streams into the agent's `task` tool-call entry in
//! the transcript, so a row carries that entry's id and a click jumps to it
//! rather than duplicating its output here.

use std::sync::Mutex;

use hrdr_tools::BackgroundTask;

/// A running blocking `task` sub-agent, shown live in the sub-agent panel until
/// it finishes. `log` is the full streamed progress/output; the panel shows only
/// its first line, as the row's title.
#[derive(Default, Clone)]
pub struct SubAgentLog {
    /// The task tool-call id (matches the `ToolOutput`/`ToolEnd` id, and the
    /// transcript entry a click on this row jumps to).
    pub id: String,
    /// Accumulated live output (starts with the `↳ task …` header line).
    pub log: String,
}

/// Stateful holder for the live list of blocking sub-agents, updated by the
/// event-fold methods as `ToolStart`/`ToolOutput`/`ToolEnd` events arrive.
#[derive(Default)]
pub struct SubAgentPanel {
    /// Live blocking sub-agents in arrival order.
    pub agents: Vec<SubAgentLog>,
}

impl SubAgentPanel {
    /// A `task` tool call started: push a new live entry.
    pub fn on_tool_start(&mut self, id: String) {
        self.agents.push(SubAgentLog {
            id,
            log: String::new(),
        });
    }

    /// Streamed output chunk for `id`: append to the matching entry's log.
    pub fn on_tool_output(&mut self, id: &str, chunk: &str) {
        if let Some(sa) = self.agents.iter_mut().find(|s| s.id == id) {
            sa.log.push_str(chunk);
        }
    }

    /// A `task` tool call ended: remove it from the live panel (its result is
    /// now in the transcript entry).
    pub fn on_tool_end(&mut self, id: &str) {
        self.agents.retain(|s| s.id != id);
    }

    /// Clear all live entries (e.g. at turn end, in case an interrupted turn
    /// left entries without a matching `ToolEnd`).
    pub fn clear(&mut self) {
        self.agents.clear();
    }
}

/// One row in the sub-agent panel: a blocking sub-agent or a detached background
/// task, unified for rendering. Exactly one screen row.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelItem {
    /// First line of the log, used as the row's title.
    pub title: String,
    /// `true` for a finished background task (renders with a completion marker).
    pub done: bool,
    /// The `task` tool-call id this row came from, so a click can jump to that
    /// entry in the transcript. `None` when the spawn had no call context.
    pub tool_id: Option<String>,
}

/// The row's text: a status marker then the title. The marker is `✓` for a
/// finished task, else `running_marker` (the frontend passes its animated
/// spinner frame). The log line's leading `↳` is stripped — the marker replaces
/// it — so a running row animates in place instead of showing a static arrow.
pub fn panel_item_header(item: &PanelItem, running_marker: &str) -> String {
    let title = item.title.trim_start_matches('↳').trim_start();
    let marker = if item.done { "✓" } else { running_marker };
    format!("{marker} {title}")
}

/// Collect the panel's rows: blocking sub-agents from `agents` followed by
/// detached background tasks from the shared registry.
pub fn panel_items(
    agents: &[SubAgentLog],
    background: &Mutex<Vec<BackgroundTask>>,
) -> Vec<PanelItem> {
    let mut items = Vec::new();
    for sa in agents {
        items.push(PanelItem {
            title: sa
                .log
                .lines()
                .next()
                .unwrap_or("sub-agent…")
                .trim()
                .to_string(),
            done: false,
            tool_id: Some(sa.id.clone()),
        });
    }
    if let Ok(v) = background.lock() {
        for t in v.iter() {
            items.push(PanelItem {
                title: t.log.lines().next().unwrap_or(&t.label).trim().to_string(),
                done: t.done,
                tool_id: t.tool_id.clone(),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_tools::BackgroundTask;

    fn make_task(id: u64, label: &str, log: &str, done: bool) -> BackgroundTask {
        BackgroundTask {
            id,
            tool_id: Some(format!("call-{id}")),
            label: label.to_string(),
            log: log.to_string(),
            done,
            result: None,
            delivered: false,
            ..Default::default()
        }
    }

    #[test]
    fn event_fold_lifecycle() {
        let mut panel = SubAgentPanel::default();
        // Start two agents.
        panel.on_tool_start("id1".to_string());
        panel.on_tool_start("id2".to_string());
        assert_eq!(panel.agents.len(), 2);
        // Stream output to the first.
        panel.on_tool_output("id1", "header line\nsecond line");
        assert_eq!(panel.agents[0].log, "header line\nsecond line");
        // End the first: removed from the live list.
        panel.on_tool_end("id1");
        assert_eq!(panel.agents.len(), 1);
        assert_eq!(panel.agents[0].id, "id2");
        // Clear on turn end.
        panel.clear();
        assert!(panel.agents.is_empty());
    }

    /// One row per agent, blocking first, each carrying the tool-call id its row
    /// jumps to — and only the log's first line, never its body.
    #[test]
    fn panel_items_merges_blocking_and_background() {
        let mut panel = SubAgentPanel::default();
        panel.on_tool_start("block1".to_string());
        panel.on_tool_output("block1", "task: do thing\nrunning…");

        let bg = Mutex::new(vec![make_task(10, "bg-label", "bg task log\nmore", true)]);

        let items = panel_items(&panel.agents, &bg);
        assert_eq!(
            items,
            vec![
                PanelItem {
                    title: "task: do thing".to_string(),
                    done: false,
                    tool_id: Some("block1".to_string()),
                },
                PanelItem {
                    title: "bg task log".to_string(),
                    done: true,
                    tool_id: Some("call-10".to_string()),
                },
            ]
        );
    }

    /// A background task spawned without a call context still renders; it just
    /// has nothing to jump to.
    #[test]
    fn a_background_task_without_a_call_id_has_no_jump_target() {
        let mut t = make_task(1, "l", "log", false);
        t.tool_id = None;
        let bg = Mutex::new(vec![t]);
        let items = panel_items(&[], &bg);
        assert_eq!(items[0].tool_id, None);
    }

    /// A finished task shows `✓`; a running one shows the frontend's animated
    /// marker in place of the log line's leading `↳`.
    #[test]
    fn the_header_marks_running_and_finished_tasks() {
        let item = |done| PanelItem {
            title: "↳ task#1".to_string(),
            done,
            tool_id: None,
        };
        assert_eq!(panel_item_header(&item(false), "⠋"), "⠋ task#1");
        assert_eq!(panel_item_header(&item(true), "⠋"), "✓ task#1");
        // A title without the arrow just gets the marker prepended.
        let plain = PanelItem {
            title: "task#2".to_string(),
            done: false,
            tool_id: None,
        };
        assert_eq!(panel_item_header(&plain, "⠙"), "⠙ task#2");
    }
}
