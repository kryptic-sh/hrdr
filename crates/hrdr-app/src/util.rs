//! Representation-independent helpers shared by hrdr's frontends: path
//! resolution, `@file` discovery (gitignore-aware), git branch + cwd display,
//! and small argument parsers (`/goto` durations, `/copy` message ranges,
//! fenced-code extraction). No UI, no rendering — pure logic + filesystem.

use std::path::{Path, PathBuf};

/// Strip trailing punctuation that may follow a token in running prose
/// (`, . ; : ) ] }`) — an `@mention`, a `todo#N`/`task#N` reference — leaving
/// the resolvable name intact.
fn trim_token(s: &str) -> &str {
    s.trim_end_matches([',', '.', ';', ':', ')', ']', '}'])
}

/// Expand `@path` mentions in `input` (resolved under `cwd`) for sending to the
/// model: a **file** contributes its contents, truncated at
/// [`MAX_ATTACH_BYTES`] on a char boundary; a **directory** contributes a
/// one-level listing of what it holds, since a directory is a pointer at files
/// to read rather than content itself.
/// Each distinct path is attached once under a trailing "Referenced paths"
/// section; unreadable/missing/duplicate references are skipped. Returns `input`
/// unchanged when nothing resolves. The display copy should keep the bare
/// `@path`; only the sent copy carries the expansion.
/// If `input` mentions a known agent via `@name` (matching one of `names`,
/// case-insensitively), return `(canonical_name, input_without_that_token)`.
/// `@`-tokens that don't match an agent are left alone (they may be `@file`
/// mentions). Only the first match is honored.
pub fn extract_agent_mention(input: &str, names: &[String]) -> Option<(String, String)> {
    // Track each token's byte offset through the iteration instead of
    // re-searching the input: the first occurrence of a token's bytes can live
    // inside an earlier word (`foo@explore` contains `@explore`), which a
    // `find` would splice out of the wrong place.
    let mut offset = 0;
    for raw in input.split_whitespace() {
        // split_whitespace skips the whitespace runs between tokens; mirror
        // that here (advancing by full chars) so `offset` lands on the start
        // of the current token.
        while offset < input.len() {
            let c = input[offset..].chars().next().unwrap();
            if !c.is_whitespace() {
                break;
            }
            offset += c.len_utf8();
        }
        let start = offset;
        offset += raw.len();
        let Some(tok) = raw.strip_prefix('@') else {
            continue;
        };
        let tok = trim_token(tok);
        if tok.is_empty() {
            continue;
        }
        if let Some(canon) = names.iter().find(|n| n.eq_ignore_ascii_case(tok)) {
            // Drop the matched `@name` token, then tidy only the doubled/edge
            // horizontal whitespace it leaves behind at the splice point.
            // Collapsing whitespace over the *whole* body (as a blanket
            // `split_whitespace().join(" ")` would) flattens every newline and
            // fenced code block in the message, not just the token's own
            // spacing — so only the splice site is touched, and only
            // spaces/tabs there, leaving newlines elsewhere intact.
            let end = start + raw.len();
            let before = &input[..start];
            let after = &input[end..];
            let before_trimmed = before.trim_end_matches([' ', '\t']);
            let after_trimmed = after.trim_start_matches([' ', '\t']);
            // Only re-insert a single space when *both* sides actually had
            // horizontal whitespace trimmed off (the "doubled space"
            // scenario). When a side borders a newline instead, that newline
            // is already the separator — don't add a stray space next to it.
            let both_sides_had_space =
                before_trimmed.len() != before.len() && after_trimmed.len() != after.len();
            let mut cleaned = String::with_capacity(before_trimmed.len() + after_trimmed.len() + 1);
            cleaned.push_str(before_trimmed);
            if both_sides_had_space {
                cleaned.push(' ');
            }
            cleaned.push_str(after_trimmed);
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
    expand_mentions_tracked(input, cwd).0
}

/// [`expand_mentions`], plus the paths whose **whole** content was inlined.
///
/// The second element is what lets the read-before-edit guard learn what the
/// model has already seen: an `@doc.md` mention puts the entire file in the
/// model's context, so forcing a re-read of it before an edit is pure waste
/// (transcripts show a 38 KiB doc re-read for exactly this reason). Only
/// complete inlines are listed — a truncated attachment is a partial view, and a
/// partial view must never license a blind overwrite.
pub fn expand_mentions_tracked(input: &str, cwd: &Path) -> (String, Vec<PathBuf>) {
    // (label, body, whole-file-inlined). Only a file's complete content counts as
    // inlined — a truncated file is a partial view, and a directory listing is not
    // content at all, so neither may license a blind overwrite.
    let mut attached: Vec<(String, String, bool)> = Vec::new();
    for raw in input.split_whitespace() {
        let Some(rel) = raw.strip_prefix('@') else {
            continue;
        };
        // A trailing `/` is how a directory is spelled (and what completion
        // inserts); strip it for resolution but keep the label honest below.
        let rel = trim_token(rel);
        let rel = rel.strip_suffix('/').unwrap_or(rel);
        if rel.is_empty()
            || attached
                .iter()
                .any(|(p, _, _)| p.trim_end_matches('/') == rel)
        {
            continue;
        }
        // A directory is pointed at, not inlined: attach what it contains so the
        // model can pick what to read, instead of the mention silently doing
        // nothing (which is what happened before — `read_attach_file` rejects a
        // directory, and the failure was swallowed).
        if resolve_under(cwd, rel).is_dir() {
            if let Ok(listing) = hrdr_tools::read_attach_dir(rel, cwd) {
                attached.push((format!("{rel}/"), listing, false));
            }
            continue;
        }
        let Ok(text) = hrdr_tools::read_attach_file(rel, cwd, Some(MAX_ATTACH_BYTES)) else {
            continue;
        };
        let (text, full) = if text.len() > MAX_ATTACH_BYTES {
            let end = floor_char_boundary(&text, MAX_ATTACH_BYTES);
            (format!("{}\n…[truncated]", &text[..end]), false)
        } else {
            (text, true)
        };
        attached.push((rel.to_string(), text, full));
    }
    if attached.is_empty() {
        return (input.to_string(), Vec::new());
    }
    let mut inlined = Vec::new();
    let mut out = String::from(input);
    out.push_str("\n\n--- Referenced paths (via @) ---\n");
    for (rel, text, full) in attached {
        if full {
            inlined.push(resolve_under(cwd, &rel));
        }
        // A directory's block says what it is, so a listing is never mistaken for
        // the file content the same syntax attaches.
        let what = if rel.ends_with('/') {
            " (directory listing, one level)"
        } else {
            ""
        };
        out.push_str(&format!("\n=== {rel}{what} ===\n{text}\n"));
    }
    (out, inlined)
}

/// Prepare an outgoing user message for sending to the model: expand a
/// `:skill` invocation into its prompt template (via [`crate::expand_skill`]),
/// then `@file` mentions into their contents (via [`expand_mentions`]) and,
/// when an `@agent` mention matches a known sub-agent name, wrap the body in a
/// delegation directive (via [`agent_mention_message`]). This is the canonical
/// "input → sent" transform shared by the TUI and the headless runner; the
/// display copy keeps the raw input.
pub fn prepare_outgoing(input: &str, names: &[String], cwd: &Path) -> String {
    prepare_outgoing_tracked(input, names, cwd, &[]).0
}

/// [`prepare_outgoing`], plus the paths whose whole content the sent message now
/// carries (see [`expand_mentions_tracked`]) — for the caller to hand to
/// [`hrdr_agent::Agent::mark_files_read`] so the read-before-edit guard knows the
/// model has already seen them. `todos` is the agent's TODO list, used to expand
/// `todo#N` / `task#N` references (see [`expand_todo_refs`]).
pub fn prepare_outgoing_tracked(
    input: &str,
    names: &[String],
    cwd: &Path,
    todos: &[hrdr_tools::TodoItem],
) -> (String, Vec<PathBuf>) {
    // A `:skill` template may itself carry `@file` / `@agent` mentions — they
    // get the same expansion below.
    let expanded;
    let input = if input.trim_start().starts_with(':') {
        match crate::expand_skill(
            input,
            &crate::discover_skills(cwd, hrdr_agent::ProjectInstructions::Load),
        ) {
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
        Some((agent, body)) => {
            let (body, inlined) = expand_mentions_tracked(&body, cwd);
            (
                agent_mention_message(&agent, &expand_todo_refs(&body, todos)),
                inlined,
            )
        }
        None => {
            let (body, inlined) = expand_mentions_tracked(input, cwd);
            (expand_todo_refs(&body, todos), inlined)
        }
    }
}

/// Expand `todo#N` / `task#N` mentions in `input` by appending each distinct
/// referenced item's content under a "Referenced todos" section, mirroring the
/// `@file` section style. A token is resolved against the item with `id == N`;
/// tokens that match no item (or reference an id-0, unassigned legacy item) are
/// left alone, like an unresolved `@path`. Returns `input` unchanged when
/// nothing matches.
pub fn expand_todo_refs(input: &str, todos: &[hrdr_tools::TodoItem]) -> String {
    let mut matched: Vec<&hrdr_tools::TodoItem> = Vec::new();
    for raw in input.split_whitespace() {
        let Some(num) = raw
            .strip_prefix("todo#")
            .or_else(|| raw.strip_prefix("task#"))
        else {
            continue;
        };
        let num = trim_token(num);
        let Ok(id) = num.parse::<u64>() else {
            continue;
        };
        if id == 0 {
            continue; // unassigned — nothing to reference
        }
        let Some(t) = todos.iter().find(|t| t.id == id) else {
            continue; // unknown id: leave the token alone
        };
        if !matched.iter().any(|m| m.id == id) {
            matched.push(t);
        }
    }
    if matched.is_empty() {
        return input.to_string();
    }
    let mut out = String::from(input);
    out.push_str("\n\n--- Referenced todos ---\n");
    for t in matched {
        out.push_str(&format!(
            "\n=== #{}: {} ===\n{}\n",
            t.id, t.status, t.content
        ));
    }
    out
}

/// Largest byte index `<= index` that lands on a UTF-8 char boundary of `s`.
pub use hrdr_tools::floor_char_boundary;

/// Resolve `path` against `base`: absolute paths pass through unchanged,
/// relative ones are joined onto `base`.
pub use hrdr_tools::resolve_under;

/// Display form of a directory, with the home directory collapsed to `~`.
///
/// Lives in `hrdr-agent` because skill discovery labels each skill's source
/// with it, and the agent owns skill discovery; re-exported so the frontends
/// (chrome, pickers) render paths identically.
pub use hrdr_agent::display_dir;

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
///
/// Each entry is `(path, lowercase_path)`: the lowercase form is computed once
/// here, so a keystroke's ranking pass ([`crate::rank_file_matches`]) never
/// re-lowercases the index. Memory trade: one extra String per indexed path,
/// built once per tree walk.
pub fn spawn_file_index(
    cwd: PathBuf,
    on_done: impl FnOnce(Vec<(String, String)>) + Send + 'static,
) {
    tokio::task::spawn_blocking(move || {
        on_done(
            walk_files(&cwd)
                .into_iter()
                .map(|p| {
                    let lower = p.to_ascii_lowercase();
                    (p, lower)
                })
                .collect(),
        );
    });
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

/// Collect relative paths under `root` for `@` completion — files, plus
/// **directories with a trailing `/`**, since `@dir/` attaches a listing and so
/// has to be selectable. In a git repo, honors `.gitignore`/`.ignore` (and
/// parents/global) + `.git/info/exclude` via the `ignore` crate; outside one,
/// falls back to a manual walk that skips known VCS/build and hidden directories.
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
        let Some(ft) = entry.file_type() else {
            continue;
        };
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        // `root` itself walks first and strips to an empty path — an `@` candidate
        // of "/" is meaningless.
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel = rel.to_string_lossy().replace('\\', "/");
        if ft.is_file() {
            out.push(rel);
        } else if ft.is_dir() {
            out.push(format!("{rel}/"));
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
                if let Ok(rel) = path.strip_prefix(root) {
                    // Selectable itself — `@dir/` attaches its listing.
                    out.push(format!("{}/", rel.to_string_lossy().replace('\\', "/")));
                }
                stack.push((path, depth + 1));
            } else if ft.is_file()
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
            if out.len() >= WALK_MAX_FILES {
                break;
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(out.contains("--- Referenced paths (via @) ---"));
        assert_eq!(out.matches("=== a.txt ===").count(), 1);
        assert!(out.contains("hello from a"));

        // A missing mention resolves nothing → unchanged.
        assert_eq!(expand_mentions("@nope.txt", root), "@nope.txt");
    }

    /// `@dir` attaches what the directory holds, spelled with or without the
    /// trailing slash. Before this it silently did nothing: the attach path only
    /// accepted regular files, and the rejection was swallowed, so the mention
    /// reached the model as bare text.
    #[test]
    fn expand_mentions_attaches_a_directory_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "//! lib").unwrap();

        for spelling in ["@src", "@src/"] {
            let out = expand_mentions(&format!("look at {spelling}"), root);
            assert!(
                out.contains("=== src/ (directory listing, one level) ==="),
                "{spelling} labels the block as a listing: {out}"
            );
            assert!(
                out.contains("main.rs") && out.contains("lib.rs"),
                "{spelling} lists the files: {out}"
            );
            assert!(
                out.contains("nested/"),
                "{spelling} marks subdirectories: {out}"
            );
            assert!(
                !out.contains("fn main()"),
                "a listing is not the files' content: {out}"
            );
        }

        // A directory is a pointer, not content, so it never disarms the
        // read-before-edit guard for the files inside it.
        let (_, inlined) = expand_mentions_tracked("@src", root);
        assert!(
            inlined.is_empty(),
            "a listing licenses no blind overwrite: {inlined:?}"
        );

        // Both spellings in one message describe the same directory — attach once.
        let out = expand_mentions("@src and @src/ again", root);
        assert_eq!(out.matches("=== src/").count(), 1, "{out}");
    }

    /// `@` completion offers directories too — a candidate you cannot select is a
    /// feature that doesn't exist.
    #[test]
    fn the_completion_index_offers_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/inner")).unwrap();
        std::fs::write(root.join("crates/inner/x.rs"), "").unwrap();
        std::fs::write(root.join("top.txt"), "").unwrap();

        let items = walk_files(root);
        assert!(items.contains(&"top.txt".to_string()), "{items:?}");
        assert!(
            items.contains(&"crates/inner/x.rs".to_string()),
            "{items:?}"
        );
        assert!(
            items.contains(&"crates/".to_string()) && items.contains(&"crates/inner/".to_string()),
            "directories are candidates, slash-suffixed: {items:?}"
        );
        assert!(
            !items.iter().any(|i| i == "/" || i.is_empty()),
            "the root itself is not a candidate: {items:?}"
        );
    }

    /// The paths reported alongside the expansion are the ones whose *whole*
    /// content the message now carries — that list is what disarms the
    /// read-before-edit guard, so it must contain nothing the model can't see in
    /// full. Resolved against `cwd`, so the caller can hand them straight to
    /// `Agent::mark_files_read` regardless of how the mention was spelled.
    #[test]
    fn expand_mentions_reports_only_fully_inlined_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello from a").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.txt"), "hello from b").unwrap();

        // Nothing inlined → nothing to mark.
        assert!(expand_mentions_tracked("just text", root).1.is_empty());
        assert!(expand_mentions_tracked("@nope.txt", root).1.is_empty());

        // Two distinct mentions, one repeated: each reported once, resolved.
        let (out, inlined) =
            expand_mentions_tracked("see @a.txt and @sub/b.txt, plus @a.txt again", root);
        assert!(out.contains("hello from b"));
        assert_eq!(
            inlined,
            vec![root.join("a.txt"), root.join("sub/b.txt")],
            "resolved, deduplicated, in mention order"
        );

        // Over the attach cap the content never lands in the message at all, so
        // there is nothing to mark — a partial view must not license an edit.
        let big = root.join("big.log");
        std::fs::write(&big, "x".repeat(MAX_ATTACH_BYTES + 1)).unwrap();
        let (out, inlined) = expand_mentions_tracked("look at @big.log", root);
        assert_eq!(out, "look at @big.log");
        assert!(inlined.is_empty(), "an unattached file is not a read file");
    }

    /// `prepare_outgoing_tracked` carries the inlined-path list out through both
    /// of its shapes — a plain message and one routed at an `@agent`.
    #[test]
    fn prepare_outgoing_tracked_reports_inlined_paths_through_agent_routing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("note.txt"), "the note").unwrap();
        let names = vec!["explore".to_string()];

        let (sent, inlined) = prepare_outgoing_tracked("read @note.txt", &names, root, &[]);
        assert!(sent.contains("the note"));
        assert_eq!(inlined, vec![root.join("note.txt")]);

        // Routed at a sub-agent: same expansion, same report.
        let (sent, inlined) =
            prepare_outgoing_tracked("@explore read @note.txt", &names, root, &[]);
        assert!(sent.contains("`explore`") && sent.contains("the note"));
        assert_eq!(inlined, vec![root.join("note.txt")]);
    }

    #[test]
    fn expand_todo_refs_appends_referenced_items() {
        let todos = vec![
            hrdr_tools::TodoItem {
                content: "fix the bug".to_string(),
                id: 1,
                status: "pending".to_string(),
                evidence: None,
            },
            hrdr_tools::TodoItem {
                content: "add a test".to_string(),
                id: 2,
                status: "in_progress".to_string(),
                evidence: None,
            },
        ];

        // A `todo#N` / `task#N` mix, each resolved against its item.
        let out = expand_todo_refs("do task#1 then todo#2", &todos);
        assert!(out.starts_with("do task#1 then todo#2"), "{out}");
        assert!(out.contains("--- Referenced todos ---"), "{out}");
        assert!(out.contains("=== #1: pending ==="), "{out}");
        assert!(out.contains("fix the bug"), "{out}");
        assert!(out.contains("=== #2: in_progress ==="), "{out}");
        assert!(out.contains("add a test"), "{out}");

        // Trailing punctuation on the token is tolerated.
        let out = expand_todo_refs("see todo#2.", &todos);
        assert!(out.contains("=== #2: in_progress ==="), "{out}");

        // Duplicate references append the item once.
        let out = expand_todo_refs("todo#2 and again todo#2", &todos);
        assert_eq!(out.matches("=== #2: in_progress ===").count(), 1, "{out}");

        // An unknown number is left alone, like an unresolved `@path`.
        assert_eq!(expand_todo_refs("see todo#99", &todos), "see todo#99");
        // A non-numeric suffix too.
        assert_eq!(expand_todo_refs("see todo#x", &todos), "see todo#x");
        // id 0 is unassigned — nothing to reference.
        assert_eq!(expand_todo_refs("see todo#0", &todos), "see todo#0");
        // A plain message without tokens is unchanged.
        assert_eq!(expand_todo_refs("just some text", &todos), "just some text");
    }

    /// `prepare_outgoing_tracked` expands `todo#N` after `@file`, in both its
    /// shapes — a plain message and one routed at an `@agent`.
    #[test]
    fn prepare_outgoing_tracked_expands_todo_refs_after_file_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("note.txt"), "the note").unwrap();
        let names = vec!["explore".to_string()];
        let todos = vec![hrdr_tools::TodoItem {
            content: "add a test".to_string(),
            id: 2,
            status: "in_progress".to_string(),
            evidence: None,
        }];

        let (sent, _) =
            prepare_outgoing_tracked("read @note.txt then todo#2", &names, root, &todos);
        // The todo section comes after the @file section.
        let file_at = sent.find("--- Referenced paths (via @) ---").unwrap();
        let todo_at = sent.find("--- Referenced todos ---").unwrap();
        assert!(todo_at > file_at, "{sent}");
        assert!(sent.contains("add a test"), "{sent}");

        // Routed at a sub-agent: same expansion, wrapped in the directive.
        let (sent, _) = prepare_outgoing_tracked("@explore do todo#2", &names, root, &todos);
        assert!(
            sent.contains("`explore`") && sent.contains("add a test"),
            "{sent}"
        );
        assert!(sent.contains("--- Referenced todos ---"), "{sent}");
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
    fn extract_agent_mention_splices_the_matched_token_not_the_first_occurrence() {
        let names = vec!["explore".to_string()];
        // `foo@explore` contains the token's bytes but is not a mention; the real
        // mention is the last token. The old code spliced `input.find("@explore")`
        // — the occurrence inside the email — gutting it, keeping the mention, and
        // jamming the words ("contact foothen @explore run audit").
        let input = "contact foo@explore then @explore run audit";
        let (a, body) = extract_agent_mention(input, &names).unwrap();
        assert_eq!(a, "explore");
        assert_eq!(body, "contact foo@explore then run audit");
    }

    /// Stripping the `@name` token must only tidy the whitespace it leaves
    /// behind at its own splice point — not collapse the whole message onto
    /// one line. A blanket `split_whitespace().join(" ")` cleanup used to
    /// flatten newlines and fenced code blocks anywhere in the body.
    #[test]
    fn extract_agent_mention_preserves_newlines_and_code_fences() {
        let names = vec!["bot".to_string()];
        let input = "@bot fix this:\n```\nfn main(){}\n```";
        let (a, body) = extract_agent_mention(input, &names).unwrap();
        assert_eq!(a, "bot");
        assert_eq!(body, "fix this:\n```\nfn main(){}\n```");

        // A mention in the middle of a line still collapses just its own
        // doubled space, not anything else in the message.
        let input = "hello @bot world\nsecond line stays intact";
        let (_, body) = extract_agent_mention(input, &names).unwrap();
        assert_eq!(body, "hello world\nsecond line stays intact");
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

    /// `@file` mentions are no longer confined to cwd (full-access default): a
    /// `..` escape and an absolute path both attach their contents.
    #[test]
    fn expand_mentions_accepts_paths_outside_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("leak.txt");
        std::fs::write(&outside, "outside content").unwrap();

        // Relative `..` escape.
        let out = expand_mentions("check @../leak.txt", &root);
        assert!(out.starts_with("check @../leak.txt"));
        assert!(out.contains("outside content"), "got: {out}");

        // Absolute path outside cwd.
        let out = expand_mentions(&format!("check @{}", outside.display()), &root);
        assert!(out.contains("outside content"), "got: {out}");
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
