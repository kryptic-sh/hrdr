//! Free helper functions with no `App` receiver.

use std::collections::HashMap;
use std::time::SystemTime;

use hrdr_agent::Todo;
use tokio::sync::mpsc;

/// Set up an OS-level watch on the config file, pinging `()` on the returned
/// channel whenever it changes. Returns `None` if a watcher can't be created
/// (the caller falls back to mtime polling). The watcher must be kept alive for
/// the watch to stay active.
pub(crate) fn setup_config_watcher()
-> Option<(notify::RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    use notify::{RecursiveMode, Watcher};
    let path = hrdr_agent::config_file_path()?;
    let dir = path.parent()?.to_path_buf();
    // Watch the parent directory (so atomic saves via rename are caught) and
    // filter to our file. Create the dir so the watch can be established.
    let _ = std::fs::create_dir_all(&dir);
    let file_name = path.file_name()?.to_os_string();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(file_name.as_os_str()))
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some((watcher, rx))
}
/// Modified-time of the user config file, for the hot-reload dedup guard.
pub(super) fn current_config_mtime() -> Option<SystemTime> {
    hrdr_agent::config_file_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}
/// Current local time, for per-message timestamps.
pub(super) fn timestamp_now() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}
/// Max input-history entries kept (in memory and on disk).
pub(super) const MAX_HISTORY: usize = 200;
/// Path to the persisted input history (`$XDG_DATA_HOME/hrdr/history`).
fn history_path() -> Option<std::path::PathBuf> {
    hjkl_xdg::data_dir("hrdr").ok().map(|d| d.join("history"))
}
/// Load persisted single-line input history (most recent `MAX_HISTORY`).
pub(super) fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if v.len() > MAX_HISTORY {
        let drop = v.len() - MAX_HISTORY;
        v.drain(0..drop);
    }
    v
}
/// Persist input history (one entry per line; multi-line entries are skipped to
/// keep the line-based file well-formed).
pub(super) fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = history
        .iter()
        .filter(|s| !s.contains('\n'))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, body);
}
/// Age out finished TODO items in place. Stamps each completed item with the
/// `turn` it was first seen finished (in `stamps`, keyed by content), then drops
/// any completed item that has been finished for `ttl` turns. Stamps for items
/// no longer present as completed are forgotten, so a re-completed item ages
/// from scratch. Pending / in-progress items are kept.
pub(super) fn age_completed_todos(
    todos: &mut Vec<Todo>,
    stamps: &mut HashMap<String, u64>,
    turn: u64,
    ttl: u64,
) {
    for t in todos.iter() {
        if t.status == "completed" {
            stamps.entry(t.content.clone()).or_insert(turn);
        }
    }
    todos.retain(|t| {
        t.status != "completed"
            || stamps
                .get(&t.content)
                .is_none_or(|&done| turn.saturating_sub(done) < ttl)
    });
    stamps.retain(|content, _| {
        todos
            .iter()
            .any(|t| t.status == "completed" && &t.content == content)
    });
}
/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry args (e.g. `code -w`), split on whitespace.
pub(crate) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
}
