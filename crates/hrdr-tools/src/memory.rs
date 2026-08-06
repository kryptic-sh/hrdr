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

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

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

/// The kinds of memory, in the order they appear in the index.
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
#[derive(Clone)]
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
                // An existing file that a hand-edit or sibling session drifted
                // from the tool's format is preserved as a backup before the
                // rewrite clobbers it.
                let backup = if file.exists() {
                    let existing = std::fs::read_to_string(&file)?;
                    backup_if_drifted(&file, &existing, &slug)?
                } else {
                    None
                };
                std::fs::write(&file, emit_memory(&mem))?;
                rebuild_index(root)?;
                match backup {
                    Some(bak) => Ok(format!(
                        "saved {scope} memory '{slug}' (type: {}) — preserved a hand-edited file as {bak}",
                        mem.mem_type.as_str()
                    )),
                    None => Ok(format!(
                        "saved {scope} memory '{slug}' (type: {})",
                        mem.mem_type.as_str()
                    )),
                }
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
                let backup = backup_if_drifted(&file, &existing, &slug)?;
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
                match backup {
                    Some(bak) => Ok(format!(
                        "updated {scope} memory '{slug}' — preserved a hand-edited file as {bak}"
                    )),
                    None => Ok(format!("updated {scope} memory '{slug}'")),
                }
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

/// Detect external drift on a memory file about to be rewritten by the tool,
/// and preserve the drifted content instead of clobbering it.
///
/// A file written by this tool round-trips exactly:
/// `emit_memory(&parse_memory(content, stem)) == content`. A human hand-edit
/// or a sibling hrdr session's rewrite breaks that round-trip. When `content`
/// still round-trips (or the file does not exist — nothing to preserve),
/// return `Ok(None)`. Otherwise copy the file to a `<stem>.<unix_ts>.bak` name
/// in the same directory and return `Ok(Some(<backup file name>))`.
///
/// The backup name MUST NOT end in `.md`: [`load_memories`] loads every file
/// whose extension is `md`, so a `.bak.md` name would be loaded as a memory
/// and appear in the index. `foo.<ts>.bak` has extension `bak` and is skipped.
///
/// Known interaction: a memory whose `description` contains a newline does not
/// round-trip (a separate open bug, deliberately NOT fixed here) and therefore
/// trips the guard, making a backup on each edit — acceptable, out of scope.
fn backup_if_drifted(file: &Path, content: &str, stem: &str) -> Result<Option<String>> {
    if emit_memory(&parse_memory(content, stem)) == content {
        return Ok(None);
    }
    let unix_ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_name = format!("{stem}.{unix_ts}.bak");
    let backup = file.with_file_name(&backup_name);
    if let Err(e) = std::fs::copy(file, &backup) {
        bail!("refusing to overwrite hand-edited memory '{stem}' — could not back it up: {e}");
    }
    Ok(Some(backup_name))
}

/// Per-root parsed-memory cache: scope root → file stem → (mtime, memory).
type MemoryCache = HashMap<PathBuf, HashMap<String, (SystemTime, Memory)>>;

/// In-process cache of parsed memories, keyed by scope root and then file stem,
/// and guarded by each file's mtime. [`load_memories`] runs on every recall,
/// search and listing, so an unchanged memory file must not be re-read and
/// re-parsed on every call — checking a file's mtime is a cheap stat.
///
/// Keying on the FILE's mtime (not the directory's) is required: a content edit
/// to an existing file does not change its directory's mtime. The key is sound
/// only where two rapid writes are distinguishable — a root on a coarse-mtime
/// filesystem is not cached at all (see [`mtime_granularity_is_fine`]).
fn memory_cache() -> &'static Mutex<MemoryCache> {
    static CACHE: OnceLock<Mutex<MemoryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Memoized per-root verdicts from [`mtime_granularity_is_fine`], so the probe
/// (a few writes to a scratch file) runs once per root per process.
fn mtime_verdicts() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static VERDICTS: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    VERDICTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `root`'s filesystem can tell two rapid writes to one file apart by
/// mtime.
///
/// The parsed-memory cache keys on each file's mtime alone, which is sound only
/// when a content edit changes the file's mtime. Some environments stamp file
/// writes from a coarse clock (a VM's timer resolution, FAT's multi-second
/// ticks): two writes within one tick report the same mtime, so a same-tick
/// edit would be served stale until the tick advances. The probe writes a
/// scratch file twice in a row and asks whether the mtimes differ — if the
/// write clock cannot tell the two writes apart, the root is not cached and
/// [`load_memories`] re-reads every file each call.
///
/// The scratch file has no `.md` extension, so even a leaked probe could never
/// be loaded as a memory.
fn mtime_granularity_is_fine(root: &Path) -> bool {
    let mut verdicts = mtime_verdicts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(v) = verdicts.get(root) {
        return *v;
    }
    let probe = root.join(".hrdr-mtime-probe");
    // Three rapid writes; fine only if every adjacent pair's mtime differs, so
    // a single tick-straddling write cannot read as "fine".
    let mut mtimes: Vec<SystemTime> = Vec::new();
    for byte in *b"abc" {
        if std::fs::write(&probe, [byte]).is_err() {
            break;
        }
        if let Ok(m) = std::fs::metadata(&probe).and_then(|m| m.modified()) {
            mtimes.push(m);
        }
    }
    let fine = mtimes.len() == 3 && mtimes.windows(2).all(|w| w[0] != w[1]);
    let _ = std::fs::remove_file(&probe);
    verdicts.insert(root.to_path_buf(), fine);
    fine
}

/// Drop the cached memories for one scope root. A caller that just rewrote
/// memory files must call this before re-reading them: the cache keys on each
/// file's mtime, and a rewrite that lands within the same mtime tick (coarse
/// filesystem granularity, e.g. some Windows setups) looks unchanged and would
/// serve the stale entry — see the notes in [`load_memories`].
fn invalidate_memory_cache(root: &Path) {
    if let Ok(mut cache) = memory_cache().lock() {
        cache.remove(root);
    }
}

/// Load every memory in the scope (stem + parsed frontmatter), skipping the
/// generated index files.
fn load_memories(root: &Path) -> Vec<(String, Memory)> {
    let mut mems = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return mems;
    };
    // The mtime-only cache key is unsound on a coarse-granularity filesystem
    // (two same-tick writes are indistinguishable); such a root is not cached,
    // so a same-tick edit is never served stale (see `mtime_granularity_is_fine`).
    let cacheable = mtime_granularity_is_fine(root);
    // The names present in this enumeration, so cache entries for files deleted
    // since the last load don't linger (pruned after the loop).
    let mut present: Vec<String> = Vec::new();
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
        present.push(stem.clone());
        // A file whose mtime matches the cached one is served a clone — no
        // read, no parse. A failed stat falls through to a fresh read, matching
        // the pre-cache error tolerance.
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let cached = if cacheable {
            mtime.and_then(|m| {
                memory_cache()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(root)
                    .and_then(|files| files.get(&stem).cloned())
                    .filter(|(cached_mtime, _)| *cached_mtime == m)
            })
        } else {
            None
        };
        if let Some((_, mem)) = cached {
            mems.push((stem, mem));
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mem = parse_memory(&content, &stem);
        if cacheable && let Some(mtime) = mtime {
            memory_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_path_buf())
                .or_default()
                .insert(stem.clone(), (mtime, mem.clone()));
        }
        mems.push((stem, mem));
    }
    // Drop cached entries whose file no longer exists, so deletions don't leak
    // stale entries.
    let mut cache = memory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(files) = cache.get_mut(root) {
        files.retain(|stem, _| present.contains(stem));
        if files.is_empty() {
            cache.remove(root);
        }
    }
    mems
}

/// Rebuild `MEMORY.md` from the memory files: pointers grouped by type (user,
/// feedback, project, reference), sorted by name within each group.
fn rebuild_index(root: &Path) -> Result<()> {
    // A mutation just rewrote memory files. The mtime cache keys on the file's
    // mtime alone, and a rewrite can land within the same tick on a
    // coarse-granularity filesystem — so drop the cached entries first, or the
    // index is rebuilt from the stale pre-mutation state (see
    // [`invalidate_memory_cache`]).
    invalidate_memory_cache(root);
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
            let piece = &entry[..crate::floor_char_boundary(&entry, budget)];
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

    /// Paths of the drift-guard backups (`*.bak`) under `root`, sorted. The
    /// timestamp in the backup name is unknowable in advance, so tests locate
    /// backups by scanning for the suffix instead.
    fn backup_paths(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let name = fname.to_str().unwrap_or("");
                if name.ends_with(".bak") {
                    out.push(entry.path());
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn edit_preserves_a_hand_edited_file_as_backup() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({
                "action": "write",
                "name": "deploy",
                "description": "original description"
            }),
            &ctx,
        )
        .await
        .unwrap();

        // A human appends a marker line directly to the file. The write had no
        // body, so the marker sits flush against the closing `---`; the tool's
        // emitter expects a blank line there, so this content no longer
        // round-trips — exactly the external drift the guard must catch.
        let file = dir.path().join("project").join("deploy.md");
        let mut hand_edited = std::fs::read_to_string(&file).unwrap();
        hand_edited.push_str("# hand edit\n");
        std::fs::write(&file, &hand_edited).unwrap();

        // A tool edit must not clobber the hand edit: it parses the current
        // content, so the marker survives, and the drifted original is backed up.
        let out = tool
            .execute(
                json!({"action": "edit", "name": "deploy", "description": "new description"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("preserved a hand-edited file as"), "{out}");

        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(
            rewritten.contains("description: new description"),
            "{rewritten}"
        );
        assert!(rewritten.contains("# hand edit"), "{rewritten}");

        let backup = backup_paths(&dir.path().join("project"));
        assert_eq!(backup.len(), 1, "exactly one backup expected");
        let bak = std::fs::read_to_string(&backup[0]).unwrap();
        assert!(bak.contains("# hand edit"), "{bak}");
        assert!(bak.contains("original description"), "{bak}");
    }

    #[tokio::test]
    async fn write_preserves_an_existing_hand_edited_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({"action": "write", "name": "deploy", "description": "old"}),
            &ctx,
        )
        .await
        .unwrap();

        // Append a marker line directly to the file. The write had no body, so
        // the marker breaks the round-trip the guard checks.
        let file = dir.path().join("project").join("deploy.md");
        let mut hand_edited = std::fs::read_to_string(&file).unwrap();
        hand_edited.push_str("# hand edit\n");
        std::fs::write(&file, &hand_edited).unwrap();

        // Write has replace semantics: the file is overwritten, but the
        // hand-edited content is preserved in a backup.
        let out = tool
            .execute(
                json!({"action": "write", "name": "deploy", "description": "brand new", "body": "replaced"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("preserved a hand-edited file as"), "{out}");

        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert!(rewritten.contains("description: brand new"), "{rewritten}");
        assert!(!rewritten.contains("# hand edit"), "{rewritten}");

        let backup = backup_paths(&dir.path().join("project"));
        assert_eq!(backup.len(), 1, "exactly one backup expected");
        let bak = std::fs::read_to_string(&backup[0]).unwrap();
        assert!(bak.contains("# hand edit"), "{bak}");
        assert!(bak.contains("description: old"), "{bak}");
    }

    #[tokio::test]
    async fn no_backup_when_the_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({"action": "write", "name": "deploy", "description": "old", "body": "step one"}),
            &ctx,
        )
        .await
        .unwrap();

        // A plain tool edit (no external change) round-trips through the
        // parser/emitter — the guard must not fire.
        let out = tool
            .execute(
                json!({"action": "edit", "name": "deploy", "description": "new"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.contains("preserved a hand-edited file"), "{out}");
        assert!(
            backup_paths(&dir.path().join("project")).is_empty(),
            "a round-tripping edit must not create a backup"
        );
    }

    #[tokio::test]
    async fn backup_files_are_not_loaded_as_memories() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        tool.execute(
            json!({"action": "write", "name": "deploy", "description": "original description"}),
            &ctx,
        )
        .await
        .unwrap();

        // Same hand edit as above: no body on the write, so the appended marker
        // trips the guard and leaves a backup behind.
        let file = dir.path().join("project").join("deploy.md");
        let mut hand_edited = std::fs::read_to_string(&file).unwrap();
        hand_edited.push_str("# hand edit\n");
        std::fs::write(&file, &hand_edited).unwrap();
        tool.execute(
            json!({"action": "edit", "name": "deploy", "description": "new description"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(backup_paths(&dir.path().join("project")).len(), 1);

        // The backup has extension `bak`, not `md`, so it is not loaded as a
        // memory and never appears in the rebuilt index.
        let proj = dir.path().join("project");
        let mems = load_memories(&proj);
        assert_eq!(mems.len(), 1, "the backup must not be loaded as a memory");
        assert_eq!(mems[0].0, "deploy");

        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(index.contains("deploy.md"), "{index}");
        assert!(!index.contains(".bak"), "{index}");
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

    #[test]
    fn load_memories_refreshes_after_write_and_reuses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "how to deploy", "step one");

        let first = load_memories(&proj);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "deploy");
        assert_eq!(first[0].1.body.trim(), "step one");

        // An unchanged second load is served from the cache (same content).
        let second = load_memories(&proj);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1.body.trim(), "step one");

        // A content edit bumps the file's mtime → cache miss → fresh parse, so
        // the cache never serves stale data after a write.
        seed(&proj, "deploy", "how to deploy", "step two");
        let third = load_memories(&proj);
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].1.body.trim(), "step two");
    }

    /// A root whose filesystem reports coarse mtime granularity must not be
    /// cached: the mtime-only key cannot tell two same-tick writes apart, so a
    /// same-tick content edit would be served stale until the tick advances.
    /// The verdict is forced by seeding the memoized probe store — the point is
    /// the bypass, not the local filesystem's (fine-grained) probe answer.
    #[test]
    fn a_coarse_mtime_root_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        std::fs::create_dir_all(&proj).unwrap();
        mtime_verdicts()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(proj.clone(), false);

        seed(&proj, "deploy", "how to deploy", "step one");
        assert_eq!(load_memories(&proj)[0].1.body.trim(), "step one");

        // A same-length edit with the mtime pinned back to the first write's
        // value — indistinguishable by an mtime-only key, and the exact shape
        // `rebuild_index_reads_a_same_tick_rewrite` exercises for the index.
        let file = proj.join("deploy.md");
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        seed(&proj, "deploy", "how to deploy", "step two");
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();

        assert_eq!(
            load_memories(&proj)[0].1.body.trim(),
            "step two",
            "a coarse-mtime root must re-read every call, never serve the cache"
        );
    }

    /// The granularity probe memoizes its verdict per root and removes its
    /// scratch file, so probing is one write per root per process and never
    /// leaves a file behind that a later load could mistake for a memory.
    #[test]
    fn mtime_probe_is_memoized_and_leaves_no_scratch_file() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        std::fs::create_dir_all(&proj).unwrap();

        let first = mtime_granularity_is_fine(&proj);
        let second = mtime_granularity_is_fine(&proj);
        assert_eq!(first, second, "the verdict must be memoized per root");
        assert!(
            !proj.join(".hrdr-mtime-probe").exists(),
            "the probe must remove its scratch file"
        );
        // The scratch name has no `.md` extension, so even a leaked probe could
        // never be loaded as a memory.
        assert!(
            load_memories(&proj).is_empty(),
            "the probe file must never read as a memory"
        );
    }

    /// A same-tick rewrite — the file's mtime pinned back to the cached value —
    /// must still reach the pointer index. The mtime cache cannot tell the two
    /// writes apart, so `rebuild_index` has to drop the cache after a mutation
    /// or a coarse-granularity filesystem (some Windows setups) serves the stale
    /// entry and the index keeps the old description. The description swap is
    /// deliberately same-length, so even a size-keyed cache wouldn't catch it.
    #[test]
    fn rebuild_index_reads_a_same_tick_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "old description", "step one");

        // Cache the pre-edit state…
        let _ = load_memories(&proj);
        // …rewrite the description, and force the mtime back to the cached one.
        let file = proj.join("deploy.md");
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("old description", "new description");
        std::fs::write(&file, text).unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();

        rebuild_index(&proj).unwrap();
        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(index.contains("— new description"), "{index}");
        assert!(!index.contains("old description"), "{index}");
    }
}
