//! The `memory` tool — durable, LLM-managed notes that persist across sessions,
//! in two scopes: **project** (this working directory) and **global** (all
//! projects). Storage roots are supplied by the caller via
//! [`ToolContext::memory_project`] / [`ToolContext::memory_global`].
//!
//! # Model
//!
//! **One memory = one `<slug>.md` file** with YAML-ish frontmatter plus a
//! Markdown body:
//!
//! ```text
//! ---
//! name: <slug>
//! description: <one line — what recall matches against>
//! type: user | feedback | project | reference
//! ---
//! <body>
//! ```
//!
//! The `type` classifies the memory: `user` (who the user is), `feedback` (a
//! correction/preference), `project` (ongoing work/constraints not in the repo),
//! `reference` (a pointer to a resource). Default `reference`.
//!
//! **`MEMORY.md` is a tool-generated pointer index**, never written by the
//! model: after every mutation the tool rebuilds it from the memory files so it
//! can't drift. It groups one-line pointers by type — this is the map loaded at
//! session start; the memories themselves stay in their files until viewed or
//! searched.
//!
//! Frontmatter is parsed and emitted by hand (this crate has no YAML dep). A
//! file with **no** frontmatter (legacy Claude Code / OKF notes) is read as
//! `type: reference`, with `description` inferred from its first non-empty line,
//! so it still lists and searches.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, truncate_saved};

pub struct MemoryTool;

#[derive(Deserialize)]
struct MemoryArgs {
    action: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "type", default)]
    mem_type: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

/// The four kinds of memory, in the order they appear in the index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MemType {
    User,
    Feedback,
    Project,
    Reference,
}

const TYPE_ORDER: [MemType; 4] = [
    MemType::User,
    MemType::Feedback,
    MemType::Project,
    MemType::Reference,
];

impl MemType {
    fn as_str(self) -> &'static str {
        match self {
            MemType::User => "user",
            MemType::Feedback => "feedback",
            MemType::Project => "project",
            MemType::Reference => "reference",
        }
    }

    /// Read a `type:` value from a file — unknown/blank falls back to `reference`
    /// so legacy and hand-edited files always classify.
    fn from_file(s: &str) -> MemType {
        Self::lookup(s).unwrap_or(MemType::Reference)
    }

    /// Parse a caller-supplied `type` argument, rejecting unknown values so a
    /// typo doesn't silently misclassify a memory.
    fn from_input(s: &str) -> Result<MemType> {
        Self::lookup(s).ok_or_else(|| {
            anyhow::anyhow!("unknown memory type '{s}' (use user, feedback, project, or reference)")
        })
    }

    fn lookup(s: &str) -> Option<MemType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(MemType::User),
            "feedback" => Some(MemType::Feedback),
            "project" => Some(MemType::Project),
            "reference" => Some(MemType::Reference),
            _ => None,
        }
    }
}

/// A parsed memory: its frontmatter fields plus the Markdown body.
struct Memory {
    name: String,
    description: String,
    mem_type: MemType,
    body: String,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> &'static str {
        "Durable, self-managed memory that persists across sessions. One memory = one small \
         file with a `name` (slug), a one-line `description` (what recall matches against), a \
         `type`, and a Markdown body. Types: `user` (who the user is), `feedback` (a correction \
         or stated preference), `project` (ongoing work or constraints not captured in the \
         repo), `reference` (a pointer to a resource); default `reference`. Two scopes: \
         `project` (this repo, default) and `global` (all projects). The `MEMORY.md` pointer \
         index is generated for you after every change — never write it yourself.\n\
         \n\
         Save memory UNPROMPTED at natural moments: the user says \"remember this\", corrects \
         you, states a durable preference, or a non-obvious project decision is made. Classify \
         it by `type`. Before writing, check for an existing memory (`search`/`view`) and \
         `edit` it instead of creating a duplicate. Prune (`delete`) a memory that a later fact \
         contradicts. Do NOT store what the repo, git history, or AGENTS.md/CLAUDE.md already \
         records, nor anything that only matters to this one conversation. Use absolute dates \
         (2026-07-23), never \"today\"/\"yesterday\".\n\
         \n\
         Actions: `view` (no `name` = the pointer index; with `name` = that memory in full), \
         `write` (create/replace a memory — needs `name` + `description`), `edit` (update only \
         the given fields of an existing memory), `delete`, `search` (rank memories by `query`)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["view", "write", "edit", "delete", "search"],
                    "description": "view (index, or one memory with `name`), write, edit, delete, or search."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "Which store — `project` (this repo, default) or `global` (all projects)."
                },
                "name": {
                    "type": "string",
                    "description": "The memory's name; slugified to its `<slug>.md` filename. Required for write/edit/delete; optional for view."
                },
                "type": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "How to classify the memory. Defaults to `reference` on write."
                },
                "description": {
                    "type": "string",
                    "description": "One line summarizing the memory — this is what recall matches against. Required on write."
                },
                "body": {
                    "type": "string",
                    "description": "The memory's Markdown body (the detail). Use absolute dates."
                },
                "query": {
                    "type": "string",
                    "description": "Substring to rank memories by (for `search`)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let a: MemoryArgs = crate::tool_args("memory", args)?;
        let scope = a.scope.as_deref().unwrap_or("project");
        let root = match scope {
            "project" => ctx.memory_project.as_ref(),
            "global" => ctx.memory_global.as_ref(),
            other => bail!("unknown memory scope '{other}' (use `project` or `global`)"),
        }
        .ok_or_else(|| {
            anyhow::anyhow!("memory is disabled (no storage directory) — enable it in config")
        })?;

        match a.action.as_str() {
            "view" => match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                None => Ok(view_index(scope, root)),
                Some(name) => {
                    let slug = safe_stem(name)?;
                    let file = resolve(root, &format!("{slug}.md"))?;
                    let text = std::fs::read_to_string(&file)
                        .map_err(|e| anyhow::anyhow!("no {scope} memory named '{slug}' ({e})"))?;
                    Ok(truncate_saved(
                        &text,
                        ctx.max_output,
                        ctx.max_output_lines,
                        crate::TruncateSide::Head,
                        "memory",
                    ))
                }
            },
            "write" => {
                let name = require_field(&a.name, "name")?;
                let slug = safe_stem(name)?;
                let description = require_field(&a.description, "description")?.to_string();
                let mem_type = match a
                    .mem_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(t) => MemType::from_input(t)?,
                    None => MemType::Reference,
                };
                let mem = Memory {
                    name: slug.clone(),
                    description,
                    mem_type,
                    body: a.body.unwrap_or_default(),
                };
                let file = resolve(root, &format!("{slug}.md"))?;
                std::fs::create_dir_all(root)?;
                std::fs::write(&file, emit_memory(&mem))?;
                rebuild_index(root)?;
                Ok(format!(
                    "saved {scope} memory '{slug}' (type: {})",
                    mem.mem_type.as_str()
                ))
            }
            "edit" => {
                let name = require_field(&a.name, "name")?;
                let slug = safe_stem(name)?;
                let file = resolve(root, &format!("{slug}.md"))?;
                let existing = std::fs::read_to_string(&file).map_err(|_| {
                    anyhow::anyhow!(
                        "no {scope} memory named '{slug}' to edit — use `write` to create it"
                    )
                })?;
                let mut mem = parse_memory(&existing, &slug);
                mem.name = slug.clone();
                if let Some(d) = a.description.filter(|d| !d.trim().is_empty()) {
                    mem.description = d;
                }
                if let Some(t) = a
                    .mem_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    mem.mem_type = MemType::from_input(t)?;
                }
                if let Some(b) = a.body {
                    mem.body = b;
                }
                std::fs::write(&file, emit_memory(&mem))?;
                rebuild_index(root)?;
                Ok(format!("updated {scope} memory '{slug}'"))
            }
            "delete" => {
                let name = require_field(&a.name, "name")?;
                let slug = safe_stem(name)?;
                let file = resolve(root, &format!("{slug}.md"))?;
                std::fs::remove_file(&file)
                    .map_err(|e| anyhow::anyhow!("deleting {scope} memory '{slug}': {e}"))?;
                rebuild_index(root)?;
                Ok(format!("deleted {scope} memory '{slug}'"))
            }
            "search" => {
                let query = require_field(&a.query, "query")?;
                Ok(search(root, query))
            }
            other => bail!("unknown memory action '{other}'"),
        }
    }
}

fn require_field<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("this action needs a non-empty `{field}`"))
}

/// Slugify a memory `name` to a safe file stem: lowercase, `[a-z0-9-]` only,
/// collapsed/trimmed dashes. Rejects path separators and empty results so a name
/// can never escape the memory root.
fn safe_stem(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("memory `name` must not be empty");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("memory name must be a simple slug, not a path (no '/' or '\\'): {name}");
    }
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("memory name '{name}' has no usable characters for a slug");
    }
    Ok(slug)
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        let lc = ch.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            out.push(lc);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Resolve `rel` under `root`, rejecting anything that isn't a plain relative
/// path so a write can't escape the memory store. (Slugs are already safe; this
/// is defense in depth.)
fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    for c in p.components() {
        if !matches!(c, Component::Normal(_)) {
            bail!("memory path must be a simple relative path (no '..' or leading '/'): {rel}");
        }
    }
    Ok(root.join(p))
}

/// Strip surrounding quotes/whitespace from a frontmatter scalar value.
fn parse_scalar(v: &str) -> String {
    let v = v.trim();
    let unquoted = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        &v[1..v.len() - 1]
    } else {
        v
    };
    unquoted.trim().to_string()
}

/// Parse a memory file's frontmatter + body. A file with no `---` frontmatter
/// block is read as `type: reference`, `description` = its first non-empty line
/// (leading `#`/`-` stripped), `name` = the given `stem`.
fn parse_memory(content: &str, stem: &str) -> Memory {
    let lines: Vec<&str> = content.lines().collect();
    let fenced = lines.first().map(|l| l.trim()) == Some("---");
    let close = fenced
        .then(|| lines.iter().skip(1).position(|l| l.trim() == "---"))
        .flatten()
        .map(|rel| rel + 1); // index of the closing `---` within `lines`
    if let Some(close) = close {
        let mut name = None;
        let mut description = None;
        let mut mem_type = None;
        for line in &lines[1..close] {
            if let Some((key, val)) = line.split_once(':') {
                match key.trim() {
                    "name" => name = Some(parse_scalar(val)),
                    "description" => description = Some(parse_scalar(val)),
                    "type" => mem_type = Some(parse_scalar(val)),
                    _ => {}
                }
            }
        }
        let body = lines[close + 1..].join("\n");
        return Memory {
            name: name
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.to_string()),
            description: description.unwrap_or_default(),
            mem_type: mem_type
                .as_deref()
                .map(MemType::from_file)
                .unwrap_or(MemType::Reference),
            body,
        };
    }
    // No frontmatter — infer from the raw content.
    let description = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches(['#', '-', ' ']).trim().to_string())
        .unwrap_or_default();
    Memory {
        name: stem.to_string(),
        description,
        mem_type: MemType::Reference,
        body: content.to_string(),
    }
}

/// Emit a memory deterministically: frontmatter (name, description, type) then
/// the body, always newline-terminated.
fn emit_memory(mem: &Memory) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", mem.name));
    out.push_str(&format!("description: {}\n", mem.description));
    out.push_str(&format!("type: {}\n", mem.mem_type.as_str()));
    out.push_str("---\n");
    let body = mem.body.trim_start_matches('\n').trim_end();
    if !body.is_empty() {
        out.push('\n');
        out.push_str(body);
        out.push('\n');
    }
    out
}

/// Load every memory in the scope (stem + parsed frontmatter), skipping the
/// generated index files.
fn load_memories(root: &Path) -> Vec<(String, Memory)> {
    let mut mems = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return mems;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if matches!(fname, "MEMORY.md" | "index.md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(fname)
            .to_string();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mem = parse_memory(&content, &stem);
        mems.push((stem, mem));
    }
    mems
}

/// Rebuild `MEMORY.md` from the memory files: pointers grouped by type (user,
/// feedback, project, reference), sorted by name within each group.
fn rebuild_index(root: &Path) -> Result<()> {
    let mems = load_memories(root);
    let mut out = String::from(
        "# Memory\n\n<!-- Generated by the `memory` tool — edit the memory files, not this index. -->\n",
    );
    for ty in TYPE_ORDER {
        let mut group: Vec<&(String, Memory)> =
            mems.iter().filter(|(_, m)| m.mem_type == ty).collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        out.push_str(&format!("\n## {}\n", ty.as_str()));
        for (stem, mem) in group {
            out.push_str(&format!(
                "- [{}]({}.md) — {}\n",
                mem.name, stem, mem.description
            ));
        }
    }
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join("MEMORY.md"), out)?;
    Ok(())
}

/// `view` with no name: return the generated pointer index, or a scope listing
/// if none exists yet.
fn view_index(scope: &str, root: &Path) -> String {
    match std::fs::read_to_string(root.join("MEMORY.md")) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => list_scope(scope, root),
    }
}

/// A plain listing of the scope's memory files (fallback when there's no index).
fn list_scope(scope: &str, root: &Path) -> String {
    let mems = load_memories(root);
    if mems.is_empty() {
        return format!("(no {scope} memory yet — save some with `memory` write)");
    }
    let mut names: Vec<&str> = mems.iter().map(|(stem, _)| stem.as_str()).collect();
    names.sort_unstable();
    let mut out = format!("{scope} memory ({}):\n", root.display());
    for name in names {
        out.push_str(&format!("- {name}.md\n"));
    }
    out
}

/// Case-insensitive relevance of one memory to an already-lowercased `needle`:
/// substring match against name + description (weighted high) and body (weighted
/// low). `0` means no match. The weighting shared by `search` (whole-query
/// substring) and `recall` (per query token).
fn relevance_score(mem: &Memory, needle: &str) -> i32 {
    let mut score = 0;
    if mem.name.to_lowercase().contains(needle) {
        score += 3;
    }
    if mem.description.to_lowercase().contains(needle) {
        score += 3;
    }
    if mem.body.to_lowercase().contains(needle) {
        score += 1;
    }
    score
}

/// Rank memories by case-insensitive substring match of `query` against name +
/// description (weighted high) and body (weighted low). Returns pointers, best
/// first, or `(no matches)`.
fn search(root: &Path, query: &str) -> String {
    let q = query.to_lowercase();
    let mut hits: Vec<(i32, String, String, String)> = Vec::new(); // (score, name, description, stem)
    for (stem, mem) in load_memories(root) {
        let score = relevance_score(&mem, &q);
        if score > 0 {
            hits.push((score, mem.name, mem.description, stem));
        }
    }
    if hits.is_empty() {
        return "(no matches)".to_string();
    }
    // Best first; ties broken by name for a stable order.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = String::new();
    for (_, name, description, stem) in hits {
        out.push_str(&format!("- {name} — {description} — {stem}.md\n"));
    }
    out
}

/// The one-line prefix that opens an injected recall block, so both the model
/// and readers can tell where recalled memory begins.
const RECALL_HEADER: &str = "[relevant memory]\n";

/// Format one recalled memory for injection: its `name` + `description` header
/// followed by the full body, then a blank-line separator.
fn format_recall_entry(mem: &Memory) -> String {
    let mut s = format!("## {}", mem.name);
    let desc = mem.description.trim();
    if !desc.is_empty() {
        s.push_str(" — ");
        s.push_str(desc);
    }
    s.push('\n');
    let body = mem.body.trim();
    if !body.is_empty() {
        s.push_str(body);
        s.push('\n');
    }
    s.push('\n'); // separator between entries
    s
}

/// Common function words dropped when tokenizing a recall query, so a
/// natural-language message ("how do I deploy the widget service?") matches on
/// its meaningful terms ("deploy", "widget", "service") rather than on "how" or
/// "the". Kept small and lowercase.
const RECALL_STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "how", "what", "why", "who", "does", "did", "can", "with", "from",
    "this", "that", "your", "are", "was", "will", "have", "has", "not", "but", "get", "got", "use",
    "using", "into", "when", "where", "which", "should", "would", "could", "about", "there",
    "then", "them", "they", "our", "out", "any", "all", "its", "let", "run",
];

/// Split a recall query into deduplicated, lowercased match tokens: alphanumeric
/// runs of length ≥ 3 that aren't stopwords. Empty when the query has no
/// meaningful terms (so recall returns nothing rather than matching noise).
fn recall_tokens(query: &str) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    for raw in query.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 3 || RECALL_STOPWORDS.contains(&raw) {
            continue;
        }
        if !toks.iter().any(|t| t == raw) {
            toks.push(raw.to_string());
        }
    }
    toks
}

/// Score a memory against the recall query's tokens by summing the shared
/// name/description/body weighting over each token — so a full-sentence message
/// matches memories that share its meaningful words. `0` means no token hit.
fn recall_score(mem: &Memory, tokens: &[String]) -> i32 {
    tokens.iter().map(|t| relevance_score(mem, t)).sum()
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary.
fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Rank the project + global memories by relevance to `query` and return the
/// full text of the top matches, bounded to `max_bytes`, formatted for injection
/// — or `None` when memory is disabled/empty or nothing matches.
///
/// This is per-turn **relevance recall**: the always-loaded pointer index tells
/// the model *what* it knows; this hands it the full facts most relevant to the
/// current message. Ranking reuses the same case-insensitive name/description/body
/// weighting as the `search` action, applied per meaningful query token so a
/// full-sentence user message matches the memories sharing its terms (an actual
/// token match is required — unrelated memories are never returned). Best-effort
/// throughout: an unreadable/unparsable file is skipped, never fails recall.
pub fn recall(
    project: Option<&Path>,
    global: Option<&Path>,
    query: &str,
    max_bytes: usize,
) -> Option<String> {
    if max_bytes <= RECALL_HEADER.len() {
        return None;
    }
    let tokens = recall_tokens(query);
    if tokens.is_empty() {
        return None;
    }

    // Collect matches across BOTH scopes; `load_memories` already skips the
    // generated index files and swallows per-file read errors.
    let mut hits: Vec<(i32, Memory)> = Vec::new();
    for root in [project, global].into_iter().flatten() {
        for (_, mem) in load_memories(root) {
            let score = recall_score(&mem, &tokens);
            if score > 0 {
                hits.push((score, mem));
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    // Best first; ties broken by name for a stable order.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    let mut out = String::from(RECALL_HEADER);
    let mut wrote = false;
    for (_, mem) in &hits {
        let entry = format_recall_entry(mem);
        if out.len() + entry.len() <= max_bytes {
            out.push_str(&entry);
            wrote = true;
        } else {
            // Truncate the last entry to whatever budget remains and stop; drop
            // it if nothing meaningful fits. Never exceed `max_bytes`.
            let budget = max_bytes - out.len();
            let piece = truncate_on_boundary(&entry, budget);
            if !piece.trim().is_empty() {
                out.push_str(piece);
                wrote = true;
            }
            break;
        }
    }
    if !wrote {
        return None;
    }
    let trimmed = out.trim_end();
    if trimmed.len() <= RECALL_HEADER.trim_end().len() {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_memory(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::new(dir);
        ctx.memory_project = Some(dir.join("project"));
        ctx.memory_global = Some(dir.join("global"));
        ctx
    }

    #[tokio::test]
    async fn write_creates_frontmattered_file_and_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({
                "action": "write",
                "name": "Prefers Tabs",
                "type": "feedback",
                "description": "User prefers tabs over spaces",
                "body": "Established 2026-07-23."
            }),
            &ctx,
        )
        .await
        .unwrap();

        // The memory file has deterministic frontmatter and a slugged name.
        let file = dir.path().join("project").join("prefers-tabs.md");
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.starts_with("---\nname: prefers-tabs\n"), "{raw}");
        assert!(raw.contains("description: User prefers tabs over spaces"));
        assert!(raw.contains("type: feedback"));
        assert!(raw.contains("Established 2026-07-23."));

        // The index has a pointer grouped under its type.
        let index = std::fs::read_to_string(dir.path().join("project").join("MEMORY.md")).unwrap();
        assert!(index.contains("## feedback"), "{index}");
        assert!(
            index.contains("- [prefers-tabs](prefers-tabs.md) — User prefers tabs over spaces"),
            "{index}"
        );
    }

    #[tokio::test]
    async fn write_requires_name_and_description() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        assert!(
            tool.execute(json!({"action": "write", "description": "d"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"action": "write", "name": "x"}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn edit_updates_field_in_place_and_resyncs_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({
                "action": "write",
                "name": "deploy",
                "type": "project",
                "description": "old description",
                "body": "step one"
            }),
            &ctx,
        )
        .await
        .unwrap();

        tool.execute(
            json!({"action": "edit", "name": "deploy", "description": "new description"}),
            &ctx,
        )
        .await
        .unwrap();

        // Body preserved, description updated in the file.
        let raw = std::fs::read_to_string(dir.path().join("project").join("deploy.md")).unwrap();
        assert!(raw.contains("description: new description"), "{raw}");
        assert!(raw.contains("step one"), "{raw}");
        assert!(!raw.contains("old description"), "{raw}");

        // Index pointer re-synced.
        let index = std::fs::read_to_string(dir.path().join("project").join("MEMORY.md")).unwrap();
        assert!(index.contains("— new description"), "{index}");
        assert!(!index.contains("old description"), "{index}");
    }

    #[tokio::test]
    async fn edit_missing_memory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let r = tool
            .execute(json!({"action": "edit", "name": "nope", "body": "x"}), &ctx)
            .await;
        assert!(r.is_err());
        assert!(format!("{}", r.unwrap_err()).contains("write"));
    }

    #[tokio::test]
    async fn delete_removes_file_and_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({"action": "write", "name": "temp", "description": "throwaway"}),
            &ctx,
        )
        .await
        .unwrap();
        assert!(dir.path().join("project").join("temp.md").exists());

        tool.execute(json!({"action": "delete", "name": "temp"}), &ctx)
            .await
            .unwrap();
        assert!(!dir.path().join("project").join("temp.md").exists());

        let index = std::fs::read_to_string(dir.path().join("project").join("MEMORY.md")).unwrap();
        assert!(!index.contains("temp.md"), "{index}");
    }

    #[tokio::test]
    async fn search_ranks_matches_and_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({"action": "write", "name": "auth", "description": "OAuth login flow", "body": "uses PKCE"}),
            &ctx,
        )
        .await
        .unwrap();
        tool.execute(
            json!({"action": "write", "name": "misc", "description": "notes", "body": "mentions oauth once"}),
            &ctx,
        )
        .await
        .unwrap();

        let out = tool
            .execute(json!({"action": "search", "query": "oauth"}), &ctx)
            .await
            .unwrap();
        // Both match; the description hit (auth) outranks the body-only hit (misc).
        let auth_pos = out.find("auth —").unwrap();
        let misc_pos = out.find("misc —").unwrap();
        assert!(auth_pos < misc_pos, "{out}");

        let none = tool
            .execute(json!({"action": "search", "query": "zzz-nothing"}), &ctx)
            .await
            .unwrap();
        assert_eq!(none.trim(), "(no matches)");
    }

    #[tokio::test]
    async fn legacy_schemaless_file_lists_and_searches_as_reference() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        // Drop a frontmatter-less file directly (as Claude Code / OKF would).
        let proj = dir.path().join("project");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("legacy.md"),
            "# Old note\nThe deploy key lives in Vault.",
        )
        .unwrap();

        // A mutation rebuilds the index; the legacy file appears under reference.
        tool.execute(
            json!({"action": "write", "name": "seed", "description": "seed"}),
            &ctx,
        )
        .await
        .unwrap();
        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(index.contains("## reference"), "{index}");
        assert!(
            index.contains("- [legacy](legacy.md) — Old note"),
            "{index}"
        );

        // And it is searchable by its body.
        let out = tool
            .execute(json!({"action": "search", "query": "vault"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("legacy.md"), "{out}");
    }

    #[tokio::test]
    async fn view_index_and_view_named() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        // Empty scope: view returns the "none yet" listing.
        let empty = tool.execute(json!({"action": "view"}), &ctx).await.unwrap();
        assert!(empty.contains("no project memory"), "{empty}");

        tool.execute(
            json!({"action": "write", "name": "who", "type": "user", "description": "is a Rustacean", "body": "prefers fish shell"}),
            &ctx,
        )
        .await
        .unwrap();

        // view (no name) returns the index.
        let index = tool.execute(json!({"action": "view"}), &ctx).await.unwrap();
        assert!(index.contains("# Memory"), "{index}");
        assert!(index.contains("## user"), "{index}");

        // view name returns the full memory (frontmatter + body).
        let full = tool
            .execute(json!({"action": "view", "name": "who"}), &ctx)
            .await
            .unwrap();
        assert!(full.contains("type: user"), "{full}");
        assert!(full.contains("prefers fish shell"), "{full}");
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        for bad in ["../escape", "/etc/passwd", "sub/../../x"] {
            let r = tool
                .execute(
                    json!({"action": "write", "name": bad, "description": "x"}),
                    &ctx,
                )
                .await;
            assert!(r.is_err(), "traversal '{bad}' must be rejected");
        }
        // Nothing escaped the scope root.
        assert!(!dir.path().join("escape.md").exists());
        assert!(
            !dir.path()
                .join("project")
                .join("..")
                .join("escape.md")
                .exists()
        );
    }

    #[tokio::test]
    async fn global_and_project_scopes_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        tool.execute(
            json!({"action": "write", "scope": "global", "name": "g", "description": "global note"}),
            &ctx,
        )
        .await
        .unwrap();
        // Project scope stays empty.
        let proj = tool
            .execute(json!({"action": "view", "scope": "project"}), &ctx)
            .await
            .unwrap();
        assert!(proj.contains("no project memory"), "{proj}");
        let glob = tool
            .execute(json!({"action": "view", "scope": "global"}), &ctx)
            .await
            .unwrap();
        assert!(glob.contains("global note"), "{glob}");
    }

    #[tokio::test]
    async fn disabled_when_no_root() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path()); // no memory dirs set
        let tool = MemoryTool;
        let r = tool.execute(json!({"action": "view"}), &ctx).await;
        assert!(r.is_err());
    }

    /// Write a memory file directly into `root` (bypassing the tool), for recall
    /// tests that don't care about the pointer index.
    fn seed(root: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(root).unwrap();
        let mem = Memory {
            name: name.to_string(),
            description: description.to_string(),
            mem_type: MemType::Reference,
            body: body.to_string(),
        };
        std::fs::write(root.join(format!("{name}.md")), emit_memory(&mem)).unwrap();
    }

    #[test]
    fn recall_ranks_match_ahead_of_nonmatch_and_returns_body() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(
            &proj,
            "deploy",
            "how to deploy the service",
            "Run the deploy script with the staging flag.",
        );
        seed(&proj, "lunch", "favorite lunch spots", "Tacos on Tuesdays.");

        let block = recall(Some(&proj), None, "how do I deploy this", 4096).unwrap();
        assert!(block.starts_with("[relevant memory]"), "{block}");
        // The matching memory's body is surfaced in full.
        assert!(
            block.contains("Run the deploy script with the staging flag."),
            "{block}"
        );
        // The unrelated memory is not returned.
        assert!(!block.contains("Tacos on Tuesdays."), "{block}");
    }

    #[test]
    fn recall_respects_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "alpha", "the alpha topic", &"A".repeat(500));
        seed(&proj, "alphabeta", "the alpha topic too", &"B".repeat(500));

        // Both match "alpha"; a tight budget must not be exceeded and must still
        // return something non-empty.
        let block = recall(Some(&proj), None, "alpha", 200).unwrap();
        assert!(block.len() <= 200, "over budget: {} bytes", block.len());
        assert!(block.contains("[relevant memory]"), "{block}");
    }

    #[test]
    fn recall_returns_none_on_no_match_disabled_or_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "how to deploy", "steps here");

        // No match.
        assert!(recall(Some(&proj), None, "unrelated-xyz", 4096).is_none());
        // Empty query.
        assert!(recall(Some(&proj), None, "   ", 4096).is_none());
        // Disabled (no roots).
        assert!(recall(None, None, "deploy", 4096).is_none());
        // Budget too small even for the header.
        assert!(recall(Some(&proj), None, "deploy", 4).is_none());
    }

    #[test]
    fn recall_searches_both_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        let glob = dir.path().join("global");
        seed(
            &proj,
            "proj-note",
            "widget configuration",
            "in project scope",
        );
        seed(
            &glob,
            "glob-note",
            "database credentials",
            "in global scope",
        );

        // A query hitting only the global memory's terms recalls from global…
        let from_global = recall(Some(&proj), Some(&glob), "database credentials", 4096).unwrap();
        assert!(from_global.contains("in global scope"), "{from_global}");
        assert!(!from_global.contains("in project scope"), "{from_global}");

        // …and a query hitting only the project memory's terms recalls from project.
        let from_project = recall(Some(&proj), Some(&glob), "widget configuration", 4096).unwrap();
        assert!(from_project.contains("in project scope"), "{from_project}");
        assert!(!from_project.contains("in global scope"), "{from_project}");
    }
}
