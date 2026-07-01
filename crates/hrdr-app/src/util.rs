//! Representation-independent helpers shared by hrdr's frontends: path
//! resolution, `@file` discovery (gitignore-aware), git branch + cwd display,
//! and small argument parsers (`/goto` durations, `/copy` message ranges,
//! fenced-code extraction). No UI, no rendering — pure logic + filesystem.

use hrdr_agent::{Message, MessageRole};
use std::path::{Path, PathBuf};

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

/// Resolve `path` against `base`: absolute paths pass through unchanged,
/// relative ones are joined onto `base`.
pub fn resolve_under(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Display form of `cwd`, with the home directory collapsed to `~`.
pub fn display_dir(cwd: &Path) -> String {
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
            && let Ok(head) = std::fs::read_to_string(Path::new(p.trim()).join("HEAD"))
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
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400)
    } else {
        return None;
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
}
