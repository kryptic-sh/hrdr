//! Representation-independent helpers shared by hrdr's frontends: path
//! resolution, `@file` discovery (gitignore-aware), git branch + cwd display,
//! and small argument parsers (`/goto` durations, `/copy` message ranges,
//! fenced-code extraction). No UI, no rendering — pure logic + filesystem.

use hrdr_agent::{Message, MessageRole, Todo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Age out finished TODO items in place. Stamps each completed item with the
/// `turn` it was first seen finished (in `stamps`, keyed by content), then drops
/// any completed item that has been finished for `ttl` turns. Stamps for items
/// no longer present as completed are forgotten, so a re-completed item ages
/// from scratch. Pending / in-progress items are kept.
pub fn age_completed_todos(
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

/// A short session name derived from the first user message (first line, trimmed,
/// capped at 60 chars). Falls back to `"untitled"` when there's no usable text.
pub fn session_name_from(msgs: &[Message]) -> String {
    msgs.iter()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| m.content.as_deref())
        .map(|c| {
            c.lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(60)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "untitled".to_string())
}

/// Expand `@file` mentions in `input` by appending the referenced files'
/// contents (resolved under `cwd`), for sending to the model. Each distinct
/// readable `@path` is attached once under a trailing "Referenced files"
/// section, truncated at 100 KiB on a char boundary; unreadable/missing/
/// duplicate references are skipped. Returns `input` unchanged when nothing
/// resolves. The display copy should keep the bare `@path`; only the sent copy
/// carries the expansion.
/// If `input` mentions a known agent via `@name` (matching one of `names`,
/// case-insensitively), return `(canonical_name, input_without_that_token)`.
/// `@`-tokens that don't match an agent are left alone (they may be `@file`
/// mentions). Only the first match is honored.
pub fn extract_agent_mention(input: &str, names: &[String]) -> Option<(String, String)> {
    for raw in input.split_whitespace() {
        let Some(tok) = raw.strip_prefix('@') else {
            continue;
        };
        let tok = tok.trim_end_matches([',', '.', ';', ':', ')', ']', '}']);
        if tok.is_empty() {
            continue;
        }
        if let Some(canon) = names.iter().find(|n| n.eq_ignore_ascii_case(tok)) {
            // Drop the first occurrence of the whole `@name` token, then tidy
            // the doubled/edge whitespace it leaves behind.
            let cleaned = input.replacen(raw, "", 1);
            let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
            return Some((canon.clone(), cleaned));
        }
    }
    None
}

/// Wrap a message directed at `@agent` with a directive that tells the main
/// agent to handle it by delegating to that sub-agent via the `task` tool.
pub fn agent_mention_message(agent: &str, body: &str) -> String {
    format!(
        "[Directed to the `{agent}` agent — handle this request by delegating it to the \
         `{agent}` sub-agent via the task tool (agent=\"{agent}\").]\n\n{body}"
    )
}

/// Size cap applied to file contents attached to an outgoing message —
/// `@file` mentions (truncated past this) and `/add` (rejected past this).
/// Keeps a single stray `@huge.log` or `/add huge.log` from blowing the
/// context window (or, for `/add`, silently ballooning the input past what
/// the user can see they're about to send).
pub const MAX_ATTACH_BYTES: usize = 100 * 1024;

pub fn expand_mentions(input: &str, cwd: &Path) -> String {
    let mut attached: Vec<(String, String)> = Vec::new();
    for raw in input.split_whitespace() {
        let Some(rel) = raw.strip_prefix('@') else {
            continue;
        };
        let rel = rel.trim_end_matches([',', '.', ';', ':', ')', ']', '}']);
        if rel.is_empty() || attached.iter().any(|(p, _)| p == rel) {
            continue;
        }
        let Ok(text) = hrdr_tools::read_attach_file(rel, cwd) else {
            continue;
        };
        let text = if text.len() > MAX_ATTACH_BYTES {
            let end = floor_char_boundary(&text, MAX_ATTACH_BYTES);
            format!("{}\n…[truncated]", &text[..end])
        } else {
            text
        };
        attached.push((rel.to_string(), text));
    }
    if attached.is_empty() {
        return input.to_string();
    }
    let mut out = String::from(input);
    out.push_str("\n\n--- Referenced files (via @) ---\n");
    for (rel, text) in attached {
        out.push_str(&format!("\n=== {rel} ===\n{text}\n"));
    }
    out
}

/// Prepare an outgoing user message for sending to the model: expand a
/// `:skill` invocation into its prompt template (via [`crate::expand_skill`]),
/// then `@file` mentions into their contents (via [`expand_mentions`]) and,
/// when an `@agent` mention matches a known sub-agent name, wrap the body in a
/// delegation directive (via [`agent_mention_message`]). This is the canonical
/// "input → sent" transform shared by the TUI and the headless runner; the
/// display copy keeps the raw input.
pub fn prepare_outgoing(input: &str, names: &[String], cwd: &Path) -> String {
    // A `:skill` template may itself carry `@file` / `@agent` mentions — they
    // get the same expansion below.
    let expanded;
    let input = if input.trim_start().starts_with(':') {
        match crate::expand_skill(input, &crate::discover_skills(cwd)) {
            Some(prompt) => {
                expanded = prompt;
                expanded.as_str()
            }
            None => input, // not a known skill: send verbatim
        }
    } else {
        input
    };
    match extract_agent_mention(input, names) {
        Some((agent, body)) => agent_mention_message(&agent, &expand_mentions(&body, cwd)),
        None => expand_mentions(input, cwd),
    }
}

/// Largest byte index `<= index` that lands on a UTF-8 char boundary of `s`.
pub use hrdr_tools::floor_char_boundary;

/// Resolve `path` against `base`: absolute paths pass through unchanged,
/// relative ones are joined onto `base`.
pub use hrdr_tools::resolve_under;

/// Display form of `cwd`, with the home directory collapsed to `~`.
pub fn display_dir(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => collapse_home(&s, &home),
        _ => s.into_owned(),
    }
}

/// Collapse `home` at a path boundary in `path` to `~`. A prefix match alone
/// isn't enough: `home = /home/mx` would strip the `/home/mx` off
/// `/home/mxaddict/proj` too, collapsing it to the bogus `~addict/proj`. Only
/// collapse when the match lands on a path boundary — the prefix is the whole
/// string, or the next char is a separator. Pure, so it's testable without
/// touching the process-wide `HOME`.
fn collapse_home(path: &str, home: &str) -> String {
    if let Some(rest) = path.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{rest}");
    }
    path.to_string()
}

/// Whether `needle`'s chars appear in order within `haystack` — the fuzzy
/// match shared by the picker filters (sessions, themes).
pub(crate) fn is_subsequence(needle: &[char], haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.iter().all(|&c| it.any(|h| h == c))
}

/// Modified-time of the user config file, for hot-reload dedup guards.
pub fn config_mtime() -> Option<std::time::SystemTime> {
    hrdr_agent::config_file_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

/// Keeps a config watch alive; drop it to stop watching.
///
/// Field order is the drop order, and it matters: the watcher (and the poller,
/// aborted in `Drop`) must release their `Sender` clones before `_debounce_tx`
/// does, or the debounce thread's channel never disconnects and the thread
/// leaks.
pub struct ConfigWatcherGuard {
    _watcher: Option<notify::RecommendedWatcher>,
    poller: Option<tokio::task::JoinHandle<()>>,
    _debounce_tx: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for ConfigWatcherGuard {
    fn drop(&mut self) {
        if let Some(p) = self.poller.take() {
            p.abort();
        }
    }
}

/// How long the config watcher waits for the dust to settle before reloading.
/// A single editor save emits a burst of filesystem events (truncate, write,
/// chmod, rename); reacting to each one would reload — and announce the reload —
/// several times per save.
pub const CONFIG_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

/// Coalesce a stream of change pings into one `on_change` call per quiet period.
///
/// Blocks until a ping arrives, then keeps swallowing pings until `window`
/// passes with none — so a burst of ten events 50ms apart fires exactly once,
/// `window` after the *last* of them. Returns when the sender is dropped.
fn debounce_loop(
    rx: std::sync::mpsc::Receiver<()>,
    window: std::time::Duration,
    on_change: impl Fn(),
) {
    use std::sync::mpsc::RecvTimeoutError;
    while rx.recv().is_ok() {
        // Swallow the rest of the burst; each new ping restarts the window.
        loop {
            match rx.recv_timeout(window) {
                Ok(()) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                // Watcher dropped mid-burst: fire nothing, we're shutting down.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        on_change();
    }
}

/// Watch the user config file, invoking `on_change` (from a background thread)
/// once per burst of modifications — the unified hot-reload source for both
/// frontends. Uses an OS-level watcher (inotify/FSEvents/…) on the config's
/// parent directory (so atomic saves via rename are caught) and falls back to
/// 2s mtime polling when a watcher can't be created.
///
/// Events are debounced by [`CONFIG_DEBOUNCE`], so one editor save triggers one
/// reload no matter how many filesystem events it emits. The callback should
/// just ping the frontend's channel; dedup of self-inflicted writes (persisting
/// a setting) is still the receiver's job.
pub fn watch_config(on_change: impl Fn() + Send + Sync + 'static) -> ConfigWatcherGuard {
    use notify::{RecursiveMode, Watcher};
    // Every event source pings this channel; the debounce thread is the only
    // caller of `on_change`.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || debounce_loop(rx, CONFIG_DEBOUNCE, on_change));

    let watcher = hrdr_agent::config_file_path().and_then(|path| {
        let dir = path.parent()?.to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        let file_name = path.file_name()?.to_os_string();
        let tx = tx.clone();
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
        Some(watcher)
    });
    // Fallback: poll the mtime when no OS watcher could be established.
    let poller = if watcher.is_none() {
        let tx = tx.clone();
        Some(tokio::spawn(async move {
            let mut last = config_mtime();
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tick.tick().await;
                let now = config_mtime();
                if now != last {
                    last = now;
                    if tx.send(()).is_err() {
                        return; // guard dropped
                    }
                }
            }
        }))
    } else {
        None
    };
    ConfigWatcherGuard {
        _watcher: watcher,
        poller,
        _debounce_tx: Some(tx),
    }
}

/// Build the `@file` completion index off the UI thread — [`walk_files`] can
/// touch tens of thousands of directory entries, which would stall a frame if
/// run inline. Runs on a blocking task; `on_done` receives the file list there
/// (send it back through your UI channel).
pub fn spawn_file_index(cwd: PathBuf, on_done: impl FnOnce(Vec<String>) + Send + 'static) {
    tokio::task::spawn_blocking(move || on_done(walk_files(&cwd)));
}

/// Current git branch (or short detached-HEAD sha) by walking up from `cwd` to
/// the repo root and reading `.git/HEAD`. Cheap, no subprocess.
pub fn git_branch(cwd: &Path) -> Option<String> {
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
            // A relative gitdir (submodules, worktrees) is relative to the
            // directory containing the `.git` file, not the process cwd.
            && let Ok(head) = std::fs::read_to_string(d.join(p.trim()).join("HEAD"))
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

/// The last fenced (```…```) code block in markdown `md`, without the fences.
pub fn last_fenced_block(md: &str) -> Option<String> {
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
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (digits, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        let n = s.strip_suffix('d')?;
        (n, 86_400)
    };
    digits.trim().parse::<i64>().ok().map(|v| v * mult)
}

/// Parse a message spec: `N` → `(N, N)`, or `N-M` → `(N, M)` (1-based,
/// inclusive). Returns `None` for invalid or zero/reversed ranges.
pub fn parse_msg_range(spec: &str) -> Option<(usize, usize)> {
    if let Some((a, b)) = spec.split_once('-') {
        let a: usize = a.trim().parse().ok()?;
        let b: usize = b.trim().parse().ok()?;
        (a >= 1 && b >= a).then_some((a, b))
    } else {
        let n: usize = spec.trim().parse().ok()?;
        (n >= 1).then_some((n, n))
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

/// Collect relative file paths under `root` for `@file` completion. In a git
/// repo, honors `.gitignore`/`.ignore` (and parents/global) + `.git/info/exclude`
/// via the `ignore` crate; outside one, falls back to a manual walk that skips
/// known VCS/build and hidden directories.
pub fn walk_files(root: &Path) -> Vec<String> {
    if hrdr_agent::in_git_repo(root) {
        walk_files_gitignore(root)
    } else {
        walk_files_fallback(root)
    }
}

/// Gitignore-aware walk (ripgrep's walker).
pub fn walk_files_gitignore(root: &Path) -> Vec<String> {
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
fn walk_files_fallback(root: &Path) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: &str) -> Todo {
        Todo {
            content: content.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn completed_todos_age_out_after_ttl() {
        const TTL: u64 = 5;
        let mut stamps = HashMap::new();
        let mut todos = vec![todo("a", "completed"), todo("b", "in_progress")];
        // Turn it completes and the next TTL-1 turns: still shown.
        for turn in 0..TTL {
            age_completed_todos(&mut todos, &mut stamps, turn, TTL);
            assert!(
                todos.iter().any(|t| t.content == "a"),
                "completed item should survive turn {turn}"
            );
        }
        // TTL turns after completion: pruned. The in-progress item stays.
        age_completed_todos(&mut todos, &mut stamps, TTL, TTL);
        assert!(!todos.iter().any(|t| t.content == "a"));
        assert!(todos.iter().any(|t| t.content == "b"));
        assert!(stamps.is_empty(), "stamp forgotten once the item is gone");
    }

    #[test]
    fn pending_todos_are_never_pruned() {
        const TTL: u64 = 5;
        let mut stamps = HashMap::new();
        let mut todos = vec![todo("keep", "pending")];
        for turn in 0..(TTL * 3) {
            age_completed_todos(&mut todos, &mut stamps, turn, TTL);
        }
        assert_eq!(todos.len(), 1);
        assert!(stamps.is_empty());
    }

    #[test]
    fn recompleted_item_ages_from_scratch() {
        const TTL: u64 = 5;
        let mut stamps = HashMap::new();
        // Completed at turn 0.
        let mut todos = vec![todo("x", "completed")];
        age_completed_todos(&mut todos, &mut stamps, 0, TTL);
        // Model flips it back to in_progress at turn 2 → stamp forgotten.
        todos[0].status = "in_progress".to_string();
        age_completed_todos(&mut todos, &mut stamps, 2, TTL);
        assert!(stamps.is_empty());
        // Re-completed at turn 3 → stamped at 3, so it survives through turn 7.
        todos[0].status = "completed".to_string();
        age_completed_todos(&mut todos, &mut stamps, 3, TTL);
        age_completed_todos(&mut todos, &mut stamps, 3 + TTL - 1, TTL);
        assert!(todos.iter().any(|t| t.content == "x"));
        age_completed_todos(&mut todos, &mut stamps, 3 + TTL, TTL);
        assert!(!todos.iter().any(|t| t.content == "x"));
    }

    #[test]
    fn expand_mentions_attaches_readable_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello from a").unwrap();

        // No mentions → unchanged.
        assert_eq!(expand_mentions("just text", root), "just text");

        // A readable mention is attached (trailing punctuation trimmed), once,
        // while the original line is preserved.
        let out = expand_mentions("look at @a.txt, and @a.txt again", root);
        assert!(out.starts_with("look at @a.txt, and @a.txt again"));
        assert!(out.contains("--- Referenced files (via @) ---"));
        assert_eq!(out.matches("=== a.txt ===").count(), 1);
        assert!(out.contains("hello from a"));

        // A missing mention resolves nothing → unchanged.
        assert_eq!(expand_mentions("@nope.txt", root), "@nope.txt");
    }

    #[test]
    fn extract_agent_mention_routes_known_agents_only() {
        let names = vec!["explore".to_string(), "review".to_string()];
        // A known agent is matched (case-insensitive) and stripped from the body.
        let (a, body) = extract_agent_mention("@Explore find the auth flow", &names).unwrap();
        assert_eq!(a, "explore");
        assert_eq!(body, "find the auth flow");
        // Trailing punctuation on the token is tolerated.
        let (a, _) = extract_agent_mention("hey @review, look here", &names).unwrap();
        assert_eq!(a, "review");
        // An unknown `@token` (e.g. a file mention) is left for file expansion.
        assert!(extract_agent_mention("open @src/main.rs", &names).is_none());
        assert!(extract_agent_mention("no mention here", &names).is_none());
        // The directive names the agent and carries the body.
        let msg = agent_mention_message("explore", "find X");
        assert!(msg.contains("`explore`") && msg.ends_with("find X"));
    }

    #[test]
    fn prepare_outgoing_expands_a_skill_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".hrdr/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("ship.md"), "Run the checklist for $ARGUMENTS").unwrap();

        let out = prepare_outgoing(":ship v2", &[], dir.path());
        assert_eq!(out, "Run the checklist for v2");
        // An unknown :name goes to the model verbatim.
        assert_eq!(prepare_outgoing(":nope", &[], dir.path()), ":nope");
        // A skill body's own @file mentions expand too.
        std::fs::write(dir.path().join("notes.txt"), "note body").unwrap();
        std::fs::write(skills.join("review.md"), "Review @notes.txt please").unwrap();
        let out = prepare_outgoing(":review", &[], dir.path());
        assert!(out.contains("note body"), "{out}");
    }

    #[test]
    fn prepare_outgoing_routes_and_expands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("note.txt"), "content of note").unwrap();

        let names = vec!["bot".to_string()];

        // Known @agent mention: body gets expand_mentions treatment and a routing
        // directive is prepended.
        let out = prepare_outgoing("@bot check @note.txt please", &names, root);
        assert!(
            out.contains("[Directed to the `bot` agent"),
            "delegation directive missing: {out}"
        );
        assert!(
            out.contains("content of note"),
            "@file expansion missing: {out}"
        );

        // Plain input with a resolvable @file: no delegation, just expansion.
        let out = prepare_outgoing("look at @note.txt", &names, root);
        assert!(
            !out.contains("[Directed to"),
            "no agent mention, should not route: {out}"
        );
        assert!(
            out.contains("content of note"),
            "@file expansion missing: {out}"
        );

        // No matches at all: passes through unchanged.
        let out = prepare_outgoing("just some text", &names, root);
        assert_eq!(out, "just some text");
    }

    #[test]
    fn parse_duration_specs() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("5m"), Some(300));
        assert_eq!(parse_duration("1h"), Some(3600));
        assert_eq!(parse_duration("2d"), Some(172_800));
        assert_eq!(parse_duration("5"), None); // no unit
        assert_eq!(parse_duration("xm"), None);
    }

    #[test]
    fn parse_msg_range_specs() {
        assert_eq!(parse_msg_range("3"), Some((3, 3)));
        assert_eq!(parse_msg_range("2-5"), Some((2, 5)));
        assert_eq!(parse_msg_range(" 2 - 5 "), Some((2, 5)));
        assert_eq!(parse_msg_range("0"), None); // 1-based
        assert_eq!(parse_msg_range("5-2"), None); // reversed
        assert_eq!(parse_msg_range("x"), None);
    }

    #[test]
    fn last_fenced_block_extraction() {
        let md = "intro\n```rust\nfn a() {}\n```\nmid\n```\nlast block\nline2\n```\nend";
        assert_eq!(last_fenced_block(md).as_deref(), Some("last block\nline2"));
        assert_eq!(last_fenced_block("no code here"), None);
        assert_eq!(last_fenced_block("```\n\n```"), None); // empty block
    }

    #[test]
    fn resolve_under_absolute_and_relative() {
        assert_eq!(
            resolve_under(Path::new("/base"), "/abs/x"),
            PathBuf::from("/abs/x")
        );
        assert_eq!(
            resolve_under(Path::new("/base"), "rel/x"),
            PathBuf::from("/base/rel/x")
        );
    }

    #[test]
    fn gitignore_walk_honors_nested_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A `.git` dir so the `ignore` crate applies gitignore rules.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // Root-level ignore.
        std::fs::write(root.join(".gitignore"), "root_ignored.txt\n").unwrap();
        std::fs::write(root.join("keep_root.txt"), "").unwrap();
        std::fs::write(root.join("root_ignored.txt"), "").unwrap();
        // Nested ignore in a subdirectory.
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(sub.join("keep_sub.txt"), "").unwrap();
        std::fs::write(sub.join("ignored.txt"), "").unwrap();

        let files = walk_files_gitignore(root);
        assert!(files.iter().any(|f| f == "keep_root.txt"), "{files:?}");
        assert!(files.iter().any(|f| f == "sub/keep_sub.txt"), "{files:?}");
        assert!(
            !files.iter().any(|f| f == "root_ignored.txt"),
            "root .gitignore not honored: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f == "sub/ignored.txt"),
            "nested sub/.gitignore not honored: {files:?}"
        );
    }

    // These test the pure `collapse_home` core rather than `display_dir` so they
    // never touch the process-wide `HOME` — no env mutation, no cross-test race.

    #[test]
    fn display_dir_collapses_home_at_a_path_boundary() {
        assert_eq!(collapse_home("/home/mx", "/home/mx"), "~");
        assert_eq!(collapse_home("/home/mx/proj", "/home/mx"), "~/proj");
    }

    /// Regression: a bare prefix match turned `/home/mxaddict/proj` (a sibling
    /// directory that merely starts with the same characters as HOME) into
    /// the bogus `~addict/proj` — `mx` is not a path component of
    /// `mxaddict`, so it must not collapse at all.
    #[test]
    fn display_dir_does_not_collapse_a_sibling_directory_sharing_a_prefix() {
        assert_eq!(
            collapse_home("/home/mxaddict/proj", "/home/mx"),
            "/home/mxaddict/proj"
        );
    }

    /// `@file` mention with `..` escape is gracefully skipped (mention stays, no content).
    #[test]
    fn expand_mentions_skips_dotdot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("leak.txt");
        std::fs::write(&outside, "data").unwrap();

        let out = expand_mentions("check @../leak.txt", &root);
        // Original text unchanged (mention stays in display copy).
        assert_eq!(out, "check @../leak.txt");
    }

    /// `@file` mention with absolute path is gracefully skipped.
    #[test]
    fn expand_mentions_skips_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();

        let out = expand_mentions("check @/etc/passwd", &root);
        assert_eq!(out, "check @/etc/passwd");
    }

    /// `@file` mention of a secret file is gracefully skipped.
    #[test]
    fn expand_mentions_skips_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();

        let out = expand_mentions("check @.env", &root);
        assert_eq!(out, "check @.env");
    }

    /// `@file` mention of a valid nested file works normally.
    #[test]
    fn expand_mentions_accepts_nested_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("notes.txt"), "nested content").unwrap();

        let out = expand_mentions("show @sub/notes.txt", &root);
        assert!(out.starts_with("show @sub/notes.txt"));
        assert!(out.contains("nested content"));
    }
}

/// The config-watcher debounce. `debounce_loop` is the whole of the coalescing
/// logic, and it's pure w.r.t. the filesystem — drive it with a channel.
#[cfg(test)]
mod debounce_tests {
    use super::{CONFIG_DEBOUNCE, debounce_loop};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Run `debounce_loop` on a thread with a short window; returns the sender,
    /// the fire counter, and the thread handle.
    fn spawn(
        window: Duration,
    ) -> (
        std::sync::mpsc::Sender<()>,
        Arc<AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let handle = std::thread::spawn(move || {
            debounce_loop(rx, window, || {
                h.fetch_add(1, Ordering::SeqCst);
            })
        });
        (tx, hits, handle)
    }

    /// Wait until `hits` reaches `want`, or time out. Returns the final count.
    fn wait_for(hits: &AtomicUsize, want: usize, timeout: Duration) -> usize {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if hits.load(Ordering::SeqCst) >= want {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        hits.load(Ordering::SeqCst)
    }

    /// A burst of events closer together than the window fires exactly once,
    /// after the last of them.
    ///
    /// Regression: one editor save emits several filesystem events (truncate,
    /// write, chmod, rename). Reacting per-event reloaded the config — and
    /// printed the "config reloaded" notice — a handful of times per save.
    #[test]
    fn a_burst_of_events_fires_once() {
        // A wide window relative to the 20ms gaps: on a loaded CI runner a
        // `sleep(20ms)` can overshoot by a lot, and any gap that outgrows the
        // window would fire mid-burst and fail the assert below.
        let window = Duration::from_millis(750);
        let (tx, hits, handle) = spawn(window);

        // 10 pings, 20ms apart — every gap is well inside the window.
        for _ in 0..10 {
            tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // Still quiet-period; nothing should have fired during the burst.
        assert_eq!(hits.load(Ordering::SeqCst), 0, "fired mid-burst");

        assert_eq!(wait_for(&hits, 1, Duration::from_secs(5)), 1);
        // And it stays at one — no trailing fire per swallowed event.
        std::thread::sleep(window);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "one reload per burst");

        drop(tx);
        handle.join().unwrap();
    }

    /// Two bursts separated by more than the window fire once each — debouncing
    /// must not swallow a genuinely later edit.
    #[test]
    fn separate_bursts_each_fire() {
        let window = Duration::from_millis(40);
        let (tx, hits, handle) = spawn(window);

        tx.send(()).unwrap();
        tx.send(()).unwrap();
        assert_eq!(wait_for(&hits, 1, Duration::from_secs(2)), 1);

        // Well after the first burst settled.
        std::thread::sleep(window * 3);
        tx.send(()).unwrap();
        assert_eq!(wait_for(&hits, 2, Duration::from_secs(2)), 2);

        drop(tx);
        handle.join().unwrap();
    }

    /// Dropping the sender ends the loop rather than leaking the thread —
    /// including while a burst is still pending, where the pending reload is
    /// abandoned (we're shutting down).
    #[test]
    fn dropping_the_sender_stops_the_loop() {
        // Idle → disconnect.
        let (tx, _hits, handle) = spawn(CONFIG_DEBOUNCE);
        drop(tx);
        handle.join().expect("loop exits when idle");

        // Mid-burst → disconnect.
        let (tx, hits, handle) = spawn(Duration::from_secs(30));
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        drop(tx);
        handle.join().expect("loop exits mid-burst");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "a pending reload is abandoned on shutdown, not fired"
        );
    }
}
