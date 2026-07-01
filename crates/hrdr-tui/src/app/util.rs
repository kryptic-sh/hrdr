//! Free helper functions with no `App` receiver.

use std::collections::HashMap;
use std::time::SystemTime;

use hrdr_agent::Todo;
use tokio::sync::mpsc;

/// Set up an OS-level watch on the config file, pinging `()` on the returned
/// channel whenever it changes. Returns `None` if a watcher can't be created
/// (the caller falls back to mtime polling). The watcher must be kept alive for
/// the watch to stay active.
pub(super) fn setup_config_watcher()
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
/// Display form of `cwd`, with the home directory collapsed to `~`.
pub(super) fn display_dir(cwd: &std::path::Path) -> String {
    let s = cwd.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s
}
/// Current git branch (or short detached-HEAD sha) by walking up from `cwd` to
/// the repo root and reading `.git/HEAD`. Cheap, no subprocess.
pub(super) fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let git = d.join(".git");
        if git.is_dir() {
            return std::fs::read_to_string(git.join("HEAD"))
                .ok()
                .and_then(|h| parse_head(&h));
        }
        if git.is_file()
            && let Ok(content) = std::fs::read_to_string(&git)
            && let Some(p) = content.strip_prefix("gitdir:")
            && let Ok(head) = std::fs::read_to_string(std::path::Path::new(p.trim()).join("HEAD"))
        {
            return parse_head(&head);
        }
        dir = d.parent();
    }
    None
}
fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None if !head.is_empty() => Some(head.chars().take(7).collect()),
        None => None,
    }
}
/// Max files indexed and max directory depth walked for `@file` completion.
const WALK_MAX_FILES: usize = 20_000;
const WALK_MAX_DEPTH: usize = 12;

/// Directory names skipped by the fallback walk (non-git projects).
const WALK_SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".cache",
    "dist",
    "build",
    ".next",
    "vendor",
    ".venv",
    "__pycache__",
];
/// The last fenced (```…```) code block in markdown `md`, without the fences.
pub(super) fn last_fenced_block(md: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_block = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            if in_block {
                blocks.push(std::mem::take(&mut cur));
                in_block = false;
            } else {
                in_block = true;
                cur.clear();
            }
        } else if in_block {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    blocks
        .into_iter()
        .next_back()
        .map(|b| b.trim_end().to_string())
        .filter(|b| !b.is_empty())
}
/// Parse a relative duration like `30s`, `5m`, `1h`, `2d` into seconds.
pub(super) fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (digits, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400)
    } else {
        return None;
    };
    digits.trim().parse::<i64>().ok().map(|v| v * mult)
}
/// Parse a message spec: `N` → `(N, N)`, or `N-M` → `(N, M)` (1-based,
/// inclusive). Returns `None` for invalid or zero/reversed ranges.
pub(super) fn parse_msg_range(spec: &str) -> Option<(usize, usize)> {
    if let Some((a, b)) = spec.split_once('-') {
        let a: usize = a.trim().parse().ok()?;
        let b: usize = b.trim().parse().ok()?;
        (a >= 1 && b >= a).then_some((a, b))
    } else {
        let n: usize = spec.trim().parse().ok()?;
        (n >= 1).then_some((n, n))
    }
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
/// Collect relative file paths under `root` for `@file` completion. In a git
/// repo, honors `.gitignore`/`.ignore` (and parents/global) + `.git/info/exclude`
/// via the `ignore` crate; outside one, falls back to a manual walk that skips
/// known VCS/build and hidden directories.
pub(super) fn walk_files(root: &std::path::Path) -> Vec<String> {
    if hrdr_agent::in_git_repo(root) {
        walk_files_gitignore(root)
    } else {
        walk_files_fallback(root)
    }
}
/// Gitignore-aware walk (ripgrep's walker).
pub(super) fn walk_files_gitignore(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(WALK_MAX_DEPTH))
        .hidden(true) // skip dotfiles/dotdirs
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .build();
    for entry in walker.flatten() {
        if out.len() >= WALK_MAX_FILES {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    out
}
/// Fallback walk for non-git directories: skip hidden + known build/VCS dirs.
fn walk_files_fallback(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > WALK_MAX_DEPTH || out.len() >= WALK_MAX_FILES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if name.starts_with('.') || WALK_SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if ft.is_file()
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= WALK_MAX_FILES {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}
/// Whether a submitted line is a common "quit the session" command, matched
/// across popular CLIs/REPLs/editors so users feel at home: bare `exit`/`quit`,
/// the `/exit` `/quit` `/bye` slash family, and vim's `:q` family.
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
pub(super) fn is_quit_command(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "exit"
            | "quit"
            | "q"
            | "bye"
            | "exit()"
            | "quit()"
            | "/exit"
            | "/quit"
            | "/q"
            | "/bye"
            | "/stop"
            | ":q"
            | ":q!"
            | ":qa"
            | ":qa!"
            | ":wq"
            | ":x"
            | ":exit"
            | ":quit"
    )
}
/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry args (e.g. `code -w`), split on whitespace.
pub(super) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
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
