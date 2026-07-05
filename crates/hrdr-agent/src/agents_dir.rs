//! Discovery of sub-agent definitions from **Markdown files**, compatible with
//! Claude Code (`.claude/agents/`) and opencode (`.opencode/agent/`) layouts as
//! well as hrdr's own (`.hrdr/agents/`).
//!
//! Each file is Markdown with a YAML-ish frontmatter block; the body is the
//! agent's system prompt. We parse the flat frontmatter fields we understand
//! (name/description/model/tools/knobs) and ignore anything nested — enough to
//! load real agent files from any of the three ecosystems without a YAML dep.
//!
//! Files are collected across all locations and **deduped by name** (case
//! -insensitive): the first source in precedence order wins (project before
//! user; hrdr → claude → opencode). The caller layers these over the built-in
//! agents, and `[[subagent]]` config over these.

use std::path::{Path, PathBuf};

use crate::SubagentProfile;

/// Discover agent-definition files across the Claude/opencode/hrdr locations,
/// relative to `cwd` for project scopes and the home/XDG dirs for user scopes.
/// Returns one profile per unique name (first source in precedence order wins).
pub fn discover_agent_profiles(cwd: &Path) -> Vec<SubagentProfile> {
    let mut out: Vec<SubagentProfile> = Vec::new();
    for dir in agent_dirs(cwd) {
        for profile in read_dir_profiles(&dir) {
            // First source wins: skip a name already registered.
            if out
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&profile.name))
            {
                continue;
            }
            out.push(profile);
        }
    }
    out
}

/// The agent directories to scan, in precedence order (highest first).
fn agent_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    dirs.push(cwd.join(".hrdr").join("agents"));
    dirs.push(cwd.join(".claude").join("agents"));
    dirs.push(cwd.join(".opencode").join("agent"));
    // User scopes.
    if let Some(d) = crate::config_dir() {
        dirs.push(d.join("agents")); // ~/.config/hrdr/agents
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join(".claude").join("agents"));
    }
    if let Ok(d) = hjkl_xdg::config_dir("opencode") {
        dirs.push(d.join("agent")); // ~/.config/opencode/agent
    }
    dirs
}

/// Home directory, cross-platform (`$HOME`, else `%USERPROFILE%`).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Parse every `*.md` file in `dir` (non-recursive) into a profile. Missing or
/// unreadable directories yield nothing; a malformed file is skipped.
fn read_dir_profiles(dir: &Path) -> Vec<SubagentProfile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Some(p) = parse_agent_file(&text, stem) {
            profiles.push(p);
        }
    }
    // Stable order within a directory (read_dir order is unspecified).
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// Parse one agent file (`text`) into a profile, using `filename_stem` as the
/// fallback name. Returns `None` if there's no usable content (no name and no
/// body/prompt).
pub fn parse_agent_file(text: &str, filename_stem: &str) -> Option<SubagentProfile> {
    let (fm, body) = split_frontmatter(text);
    let body = body.trim();

    let name = fm
        .get("name")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| filename_stem.to_string());
    if name.is_empty() {
        return None;
    }

    // System prompt: the body, else an inline `prompt:` frontmatter value.
    let prompt = if !body.is_empty() {
        Some(body.to_string())
    } else {
        fm.get("prompt")
            .map(|v| v.scalar())
            .filter(|s| !s.is_empty())
    };

    let description = fm
        .get("description")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty());
    let provider = fm
        .get("provider")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty());
    let model = fm
        .get("model")
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty() && s != "inherit"); // Claude's `inherit` = default

    let temperature = fm.get("temperature").and_then(|v| v.scalar().parse().ok());
    let effort = fm
        .get("effort")
        .or_else(|| fm.get("reasoningEffort"))
        .map(|v| v.scalar())
        .filter(|s| !s.is_empty());
    // Claude `maxTurns`, opencode `steps`, hrdr `max_steps`.
    let max_steps = ["max_steps", "maxTurns", "steps"]
        .iter()
        .find_map(|k| fm.get(*k))
        .and_then(|v| v.scalar().parse().ok());

    let read_only = fm
        .get("read_only")
        .map(|v| matches!(v.scalar().as_str(), "true" | "yes" | "1"))
        .unwrap_or(false);
    let write_ext = fm
        .get("write_ext")
        .map(|v| v.list())
        .filter(|l| !l.is_empty());
    // Only an allow-list form is honored (Claude/hrdr). opencode's boolean
    // `tools:` map is nested, so it parses to an empty list here and is ignored.
    let tools = fm.get("tools").map(|v| v.list()).filter(|l| !l.is_empty());

    Some(SubagentProfile {
        name,
        provider,
        model,
        description,
        prompt,
        read_only,
        tools,
        write_ext,
        temperature,
        effort,
        max_steps,
    })
}

/// A frontmatter value: a scalar, or a list (inline `[a, b]`, CSV, or `- item`
/// block). Nested maps aren't represented (their indented lines are ignored).
#[derive(Debug, Clone)]
enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

impl FmValue {
    fn scalar(&self) -> String {
        match self {
            FmValue::Scalar(s) => s.clone(),
            FmValue::List(l) => l.join(", "),
        }
    }
    fn list(&self) -> Vec<String> {
        match self {
            FmValue::List(l) => l.clone(),
            FmValue::Scalar(s) if s.is_empty() => Vec::new(),
            // A scalar in list position may be CSV (`Read, Grep`) or one item.
            FmValue::Scalar(s) => s
                .split(',')
                .map(|p| dequote(p.trim()))
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }
}

/// Split `text` into (frontmatter map, body). A leading `---` … `---` fence is
/// the frontmatter; without one, the whole text is the body.
fn split_frontmatter(text: &str) -> (std::collections::HashMap<String, FmValue>, &str) {
    let mut map = std::collections::HashMap::new();
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (map, text);
    };
    // The opening fence must be its own line.
    let rest = match rest.strip_prefix('\n') {
        Some(r) => r,
        None => return (map, text),
    };
    // Find the closing fence line (`---` on its own line).
    let Some(end) = find_closing_fence(rest) else {
        return (map, text);
    };
    let (fm_text, after) = rest.split_at(end);
    let body = after
        .trim_start_matches("---")
        .strip_prefix('\n')
        .unwrap_or("");

    parse_frontmatter(fm_text, &mut map);
    (map, body)
}

/// Byte offset of the closing `---` fence line within `s` (the start of that
/// line), or `None` if unterminated.
fn find_closing_fence(s: &str) -> Option<usize> {
    let mut offset = 0;
    for line in s.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Parse flat `key: value` frontmatter lines into `map`. Indented lines are
/// treated as belonging to the preceding key: `- item` lines build a list,
/// anything else (nested map entries) is ignored.
fn parse_frontmatter(fm: &str, map: &mut std::collections::HashMap<String, FmValue>) {
    let mut last_key: Option<String> = None;
    for raw in fm.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A list item under the previous key.
        if let Some(item) = line.strip_prefix("- ") {
            if let Some(k) = &last_key {
                let entry = map
                    .entry(k.clone())
                    .or_insert_with(|| FmValue::List(Vec::new()));
                if let FmValue::List(l) = entry {
                    l.push(dequote(item.trim()));
                } else {
                    *entry = FmValue::List(vec![dequote(item.trim())]);
                }
            }
            continue;
        }
        // Indented non-list line → part of a nested map: ignore, but keep the
        // current key so a following `- item` still attaches correctly.
        if indent > 0 {
            continue;
        }
        // A top-level `key: value`.
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        let val = v.trim();
        last_key = Some(key.clone());
        if val.is_empty() {
            // Value continues on following `- item` lines (or a nested map).
            map.entry(key).or_insert_with(|| FmValue::List(Vec::new()));
        } else if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // Inline list `[a, b]`.
            let items = inner
                .split(',')
                .map(|p| dequote(p.trim()))
                .filter(|p| !p.is_empty())
                .collect();
            map.insert(key, FmValue::List(items));
        } else {
            map.insert(key, FmValue::Scalar(dequote(val)));
        }
    }
}

/// Strip matching surrounding quotes from a scalar.
fn dequote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_style_agent() {
        let text = "---\n\
            name: code-reviewer\n\
            description: Reviews code for quality\n\
            model: sonnet\n\
            tools: Read, Grep, Bash\n\
            ---\n\
            You are a careful code reviewer.\n";
        let p = parse_agent_file(text, "fallback").unwrap();
        assert_eq!(p.name, "code-reviewer");
        assert_eq!(p.description.as_deref(), Some("Reviews code for quality"));
        assert_eq!(p.model.as_deref(), Some("sonnet"));
        assert_eq!(
            p.tools.as_deref(),
            Some(&["Read".into(), "Grep".into(), "Bash".into()][..])
        );
        assert_eq!(
            p.prompt.as_deref(),
            Some("You are a careful code reviewer.")
        );
    }

    #[test]
    fn parses_block_list_and_knobs() {
        let text = "---\n\
            description: Planner\n\
            temperature: 0.2\n\
            reasoningEffort: high\n\
            steps: 25\n\
            tools:\n\
            \x20 - Read\n\
            \x20 - Grep\n\
            ---\n\
            Body prompt.\n";
        let p = parse_agent_file(text, "planner").unwrap();
        assert_eq!(p.name, "planner"); // from filename
        assert_eq!(p.temperature, Some(0.2));
        assert_eq!(p.effort.as_deref(), Some("high"));
        assert_eq!(p.max_steps, Some(25));
        assert_eq!(
            p.tools.as_deref(),
            Some(&["Read".into(), "Grep".into()][..])
        );
    }

    #[test]
    fn ignores_nested_map_from_opencode_tools() {
        // opencode's boolean tools map is nested; we don't mistake its keys for
        // top-level frontmatter, and the empty list is dropped (tools = None).
        let text = "---\n\
            description: Build agent\n\
            mode: subagent\n\
            tools:\n\
            \x20 write: false\n\
            \x20 edit: false\n\
            model: anthropic/claude-sonnet-4\n\
            ---\n\
            Do the thing.\n";
        let p = parse_agent_file(text, "build").unwrap();
        assert_eq!(p.description.as_deref(), Some("Build agent"));
        assert_eq!(p.model.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert!(p.tools.is_none(), "nested bool-map must not become a list");
    }

    #[test]
    fn claude_inherit_model_is_treated_as_default() {
        let text = "---\nname: x\nmodel: inherit\n---\nbody\n";
        let p = parse_agent_file(text, "x").unwrap();
        assert!(p.model.is_none());
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let p = parse_agent_file("Just a system prompt.", "helper").unwrap();
        assert_eq!(p.name, "helper");
        assert_eq!(p.prompt.as_deref(), Some("Just a system prompt."));
        assert!(p.description.is_none());
    }

    #[test]
    fn discovery_dedupes_by_name_across_locations() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // Same agent name in both project .claude and project .opencode.
        let claude = cwd.join(".claude").join("agents");
        let opencode = cwd.join(".opencode").join("agent");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&opencode).unwrap();
        std::fs::write(
            claude.join("reviewer.md"),
            "---\nname: reviewer\ndescription: from claude\n---\nclaude body\n",
        )
        .unwrap();
        std::fs::write(
            opencode.join("reviewer.md"),
            "---\nname: reviewer\ndescription: from opencode\n---\nopencode body\n",
        )
        .unwrap();

        let found = discover_agent_profiles(cwd);
        let revs: Vec<&SubagentProfile> = found.iter().filter(|p| p.name == "reviewer").collect();
        assert_eq!(revs.len(), 1, "same name registered once");
        // .claude precedes .opencode in the precedence order → it wins.
        assert_eq!(revs[0].description.as_deref(), Some("from claude"));
    }
}
