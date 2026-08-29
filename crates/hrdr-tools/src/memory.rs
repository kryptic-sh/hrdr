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
//! Frontmatter is strict YAML, parsed and emitted with `serde_yaml_ng`, so a
//! `description` holding a `: `, a quote or a newline survives the round trip.
//! Frontmatter the parser rejects — an unquoted `description: repo: note`, or a
//! `---` that never closes — is an **error**, never a silent empty memory: `view`
//! and `edit` fail with the file name and the parser's own line/column, `write`
//! still replaces the file (preserving the unparsable original as a `.bak`), and
//! [`load_memories`] skips it while reporting the skip in the index, in
//! `search`, and in the scope listing. A file with **no** frontmatter (legacy
//! Claude Code / OKF notes) is a different, supported input, not malformed YAML:
//! it is read as `type: reference`, with `description` inferred from its first
//! non-empty line, so it still lists and searches.
//!
//! A `description` may therefore contain newlines. The stored value keeps them;
//! every line-oriented renderer (the `MEMORY.md` index, `search`, recall
//! headers) flattens through [`flatten_line`] at the point it prints.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
#[derive(Clone, Debug)]
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
                    // `view` returns the file verbatim, so parsing here buys
                    // nothing for the happy path — it is how the user gets the
                    // parser's complaint about a file the store is skipping.
                    parse_memory(&text, &slug).map_err(|e| unreadable_memory(&file, &e))?;
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
                // Parsed BEFORE the drift guard runs: an edit that is about to
                // fail must not leave a backup file behind for a rewrite that
                // never happens.
                let mut mem =
                    parse_memory(&existing, &slug).map_err(|e| unreadable_memory(&file, &e))?;
                let backup = backup_if_drifted(&file, &existing, &slug)?;
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

/// The error `view` and `edit` report for a file they cannot read as a memory:
/// the path plus [`parse_memory`]'s reason, which together are everything
/// needed to fix the file by hand.
fn unreadable_memory(file: &Path, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: {reason}\n(`memory` write replaces it and keeps the current content as a .bak)",
        file.display()
    )
}

fn require_field<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("this action needs a non-empty `{field}`"))
}

/// The longest slug a memory name may produce.
///
/// Every mainstream filesystem caps one path component at 255 bytes (ext4,
/// APFS, NTFS), and the tool derives names longer than `<slug>.md` from a slug —
/// a drift backup is `<slug>.<unix_ts>-<n>.bak`, tens of characters more. This
/// sits well inside the component limit so every derived form fits too. Slugs
/// are ASCII, so a character is a byte.
const MAX_SLUG_LEN: usize = 200;

/// Slugify a memory `name` to a safe file stem: lowercase, `[a-z0-9-]` only,
/// collapsed/trimmed dashes. Rejects path separators, empty results and Windows
/// device names so a name can never escape the memory root — or become a file
/// one of the supported platforms refuses to create.
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
    // Refused here rather than at the write, where the filesystem's own refusal
    // arrives as a bare `File name too long (os error 36)` naming neither the
    // tool nor the memory.
    if slug.len() > MAX_SLUG_LEN {
        bail!(
            "memory name '{name}' slugs to {} characters, past the {MAX_SLUG_LEN}-character limit \
             a file name leaves — shorten it",
            slug.len()
        );
    }
    // Refused on every platform, not only Windows, and refused rather than
    // rewritten. Rewriting (`con` → `con-memory`, say) would move an existing
    // `con.md` to a different path with nothing telling the user their memory was
    // left behind, and a store synced between machines would then hold two names
    // for one memory. The refusal names the fix instead. What it costs: a `con.md`
    // written before this check is no longer reachable through the tool — rename
    // the file on disk — but it still lists in the index, so it cannot go missing
    // in silence.
    if is_windows_device_name(&slug) {
        bail!(
            "memory name '{name}' slugs to '{slug}', which Windows reserves as a device name \
             (with any extension) — pick another name"
        );
    }
    // The two exact names the loader skips (`load_memories` — `MEMORY.md` is the
    // generated pointer index, `index.md` its sibling) — slugified, so the case
    // is already folded. Writing one of these would report success and then be
    // invisible to the index, `search` and `recall` forever; and on a
    // case-insensitive filesystem a `memory` slug resolves to the same file as
    // the generated `MEMORY.md`, so the write stomps the index and `rebuild_index`
    // stomps the memory. Refuse, as loudly as the device names.
    if matches!(slug.as_str(), "index" | "memory") {
        bail!(
            "memory name '{name}' slugs to '{slug}', which is reserved for the generated \
             pointer index (MEMORY.md) — pick another name"
        );
    }
    Ok(slug)
}

/// Whether `slug` is one of the names Windows reserves for a device: `CON`,
/// `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`. Reserved case-insensitively
/// and with any extension, so `con.md` is as unusable as `con`.
///
/// Only whole-slug matches can occur: [`slugify`] emits `[a-z0-9-]`, and turns
/// the dot of a name like `con.md` into a dash (`con-md`), which is not reserved.
fn is_windows_device_name(slug: &str) -> bool {
    if matches!(slug, "con" | "prn" | "aux" | "nul") {
        return true;
    }
    let Some(port) = slug
        .strip_prefix("com")
        .or_else(|| slug.strip_prefix("lpt"))
    else {
        return false;
    };
    matches!(port.as_bytes(), [b'1'..=b'9'])
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

/// The three frontmatter fields a memory file can carry, each absent when the
/// block does not set it.
struct ParsedFrontmatter {
    name: Option<String>,
    description: Option<String>,
    mem_type: Option<String>,
}

/// Read `name`/`description`/`type` out of a frontmatter block with strict YAML
/// parsing (`serde_yaml_ng`), so a `description` that needed quoting — one
/// holding a `: `, a newline, or leading/trailing quote characters — comes back
/// as the value that was written rather than as its first line.
///
/// `doc` is the frontmatter **including its opening `---` line and excluding the
/// closing one**, which is a single YAML document: `---` is YAML's own
/// document-start marker, so the parser accepts it and — the reason it is passed
/// in rather than stripped — counts lines from the top of the FILE. The
/// `at line L column C` in a returned error therefore points at the line the
/// user has to edit.
///
/// Errors rather than salvaging what it can. Salvage would have to guess at the
/// author's intent, and it guesses silently: the memory that comes back is one
/// nobody wrote, and the mistake that produced it is never reported and so never
/// fixed. Every file [`emit_memory`] writes parses, so a rejection means a hand
/// edit went wrong and its author is the one who can say what was meant. The two
/// failures:
///
/// * the parser rejects the block (an unquoted `description: repo: note` is a
///   YAML scanner error) — its message is returned verbatim;
/// * the block parses but is not a mapping (a bare line, a list), which cannot
///   carry `name`/`description`/`type`.
///
/// An empty block (`---` immediately followed by `---`) parses as YAML null and
/// is accepted as "no fields set", not an error: it claims nothing, so there is
/// nothing to be wrong about.
fn parse_frontmatter(doc: &str) -> Result<ParsedFrontmatter, String> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(doc).map_err(|e| {
        format!("frontmatter is not valid YAML: {e} (line/column count from the top of the file)")
    })?;
    let map = match value {
        serde_yaml_ng::Value::Mapping(map) => map,
        serde_yaml_ng::Value::Null => serde_yaml_ng::Mapping::new(),
        other => {
            return Err(format!(
                "frontmatter must be a YAML mapping of `key: value` lines, found {}",
                yaml_kind(&other)
            ));
        }
    };
    let scalar = |key: &str| map.get(key).and_then(scalar_to_string);
    Ok(ParsedFrontmatter {
        name: scalar("name"),
        description: scalar("description"),
        mem_type: scalar("type"),
    })
}

/// The name of a YAML value's kind, for the error naming what was found where a
/// mapping was required.
fn yaml_kind(v: &serde_yaml_ng::Value) -> &'static str {
    match v {
        serde_yaml_ng::Value::Null => "null",
        serde_yaml_ng::Value::Bool(_) => "a boolean",
        serde_yaml_ng::Value::Number(_) => "a number",
        serde_yaml_ng::Value::String(_) => "a plain string",
        serde_yaml_ng::Value::Sequence(_) => "a list",
        serde_yaml_ng::Value::Mapping(_) => "a mapping",
        serde_yaml_ng::Value::Tagged(_) => "a tagged value",
    }
}

/// Stringify a YAML scalar (string/number/bool) exactly as parsed. `None` for
/// `Null` or a non-scalar (sequence/mapping/tagged), none of which is a usable
/// `name`, `description` or `type`.
///
/// Deliberately not trimmed: the parser has already applied YAML's own rules
/// (a plain scalar arrives without its surrounding spaces, a quoted one keeps
/// what it quoted), and trimming on top would change a value the emitter is
/// about to write back — breaking the round trip [`backup_if_drifted`] checks.
fn scalar_to_string(v: &serde_yaml_ng::Value) -> Option<String> {
    match v {
        serde_yaml_ng::Value::String(s) => Some(s.clone()),
        serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
        serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parse a memory file's frontmatter + body, or say why it cannot be read.
///
/// A file with no `---` frontmatter block is read as `type: reference`,
/// `description` = its first non-empty line (leading `#`/`-` stripped), `name` =
/// the given `stem`. That is a supported input (legacy Claude Code / OKF notes),
/// not a malformed one.
///
/// `Err` carries a message written for the person who has to fix the file — the
/// parser's own complaint with its file line/column — and is returned when a
/// file DOES claim to have frontmatter but the claim does not hold: the block
/// opens and never closes, or [`parse_frontmatter`] rejects it. Callers name the
/// file; this only knows the reason.
fn parse_memory(content: &str, stem: &str) -> Result<Memory, String> {
    let lines: Vec<&str> = content.lines().collect();
    // A fence line carries no indentation. Only trailing whitespace is
    // tolerated, because a multi-line value is emitted as an INDENTED block
    // scalar — a description holding a line of `---` writes as `  ---`, and
    // matching that as the closing fence would cut the frontmatter in half.
    let is_fence = |l: &&str| l.trim_end() == "---";
    let fenced = lines.first().is_some_and(is_fence);
    let close = fenced
        .then(|| lines.iter().skip(1).position(is_fence))
        .flatten()
        .map(|rel| rel + 1); // index of the closing `---` within `lines`
    if fenced && close.is_none() {
        return Err(
            "frontmatter opens with `---` on line 1 but never closes — add a `---` line after \
             the last field, or remove the opening one"
                .to_string(),
        );
    }
    if let Some(close) = close {
        // The opening `---` goes to the parser with the block (see
        // `parse_frontmatter`); the closing one starts a second document and
        // must not.
        let ParsedFrontmatter {
            name,
            description,
            mem_type,
        } = parse_frontmatter(&lines[..close].join("\n"))?;
        let body = lines[close + 1..].join("\n");
        return Ok(Memory {
            name: name
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.to_string()),
            description: description.unwrap_or_default(),
            mem_type: mem_type
                .as_deref()
                .map(MemType::from_file)
                .unwrap_or(MemType::Reference),
            body,
        });
    }
    // No frontmatter — infer from the raw content.
    let description = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches(['#', '-', ' ']).trim().to_string())
        .unwrap_or_default();
    Ok(Memory {
        name: stem.to_string(),
        description,
        mem_type: MemType::Reference,
        body: content.to_string(),
    })
}

/// The frontmatter block as it is written back out. A `#[derive(Serialize)]`
/// struct rather than a [`serde_yaml_ng::Mapping`] because serde emits a
/// struct's fields in declaration order, so the field order here IS the file
/// order — nothing at the call site can reorder it, and there is no insertion
/// sequence to keep in step with it.
#[derive(Serialize)]
struct Frontmatter<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(rename = "type")]
    mem_type: &'a str,
}

/// Emit a memory deterministically: frontmatter (name, description, type) then
/// the body, always newline-terminated.
///
/// The frontmatter goes through the YAML emitter, so a value needing quotes
/// gets them — a `description` containing `: `, a newline, a leading `-`/`#`/`%`
/// or edge quote characters, or an empty one, is written in a form that reads
/// back as itself.
fn emit_memory(mem: &Memory) -> String {
    let frontmatter = serde_yaml_ng::to_string(&Frontmatter {
        name: &mem.name,
        description: &mem.description,
        mem_type: mem.mem_type.as_str(),
    })
    // Serializing three string fields into an in-memory buffer has no failure
    // path: the emitter only errors on I/O, and the writer is a `Vec<u8>`.
    .expect("a three-string mapping always serializes to YAML");
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&frontmatter);
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
/// return `Ok(None)`. Otherwise copy the file to a free `<stem>.<unix_ts>.bak`
/// name in the same directory (see [`claim_backup_name`]) and return
/// `Ok(Some(<backup file name>))`.
///
/// The backup name MUST NOT end in `.md`: [`load_memories`] loads every file
/// whose extension is `md`, so a `.bak.md` name would be loaded as a memory
/// and appear in the index. `foo.<ts>.bak` has extension `bak` and is skipped.
///
/// Content that does not PARSE cannot round-trip either, so it takes the same
/// path: `write` over a file whose frontmatter YAML rejects always leaves a
/// backup before replacing it. That is the whole reason `write` may proceed
/// where `view` and `edit` refuse — nothing is lost by replacing a file that
/// has been copied aside first.
fn backup_if_drifted(file: &Path, content: &str, stem: &str) -> Result<Option<String>> {
    if parse_memory(content, stem).is_ok_and(|mem| emit_memory(&mem) == content) {
        return Ok(None);
    }
    let unix_ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_name = claim_backup_name(file, stem, unix_ts)
        .and_then(|name| std::fs::copy(file, file.with_file_name(&name)).map(|_| name));
    match backup_name {
        Ok(name) => Ok(Some(name)),
        Err(e) => {
            bail!("refusing to overwrite hand-edited memory '{stem}' — could not back it up: {e}")
        }
    }
}

/// How many drifted copies of one memory a single second can hold before
/// [`claim_backup_name`] gives up. Reaching it means something is rewriting the
/// same memory in a loop, which the caller should hear about rather than have
/// papered over.
const MAX_BACKUPS_PER_SECOND: u32 = 100;

/// Reserve an unused `<stem>.<unix_ts>.bak` next to `file`, adding a `-1`, `-2`
/// … suffix until one is free, and return the name.
///
/// The timestamp is whole seconds, so two drift detections in the same second
/// name the same file — and a plain copy would make the second backup eat the
/// first, which is the one thing a backup must not do. The name is *claimed*
/// with `create_new` rather than tested with `exists` so that a sibling hrdr
/// session racing for the same name loses the race instead of silently winning
/// it; the caller's copy then overwrites the empty file it just created (and
/// `fs::copy` carries the source's permission bits onto it).
fn claim_backup_name(file: &Path, stem: &str, unix_ts: u64) -> std::io::Result<String> {
    for n in 0..MAX_BACKUPS_PER_SECOND {
        let name = match n {
            0 => format!("{stem}.{unix_ts}.bak"),
            n => format!("{stem}.{unix_ts}-{n}.bak"),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(file.with_file_name(&name))
        {
            Ok(_) => return Ok(name),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("{stem} already has {MAX_BACKUPS_PER_SECOND} backups from this second"),
    ))
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

/// One scope's loadable memories, plus the `.md` files that could not be loaded
/// as one.
///
/// A store with a broken file still works — one unreadable file must not take
/// the scope down — but the skip is never silent: `skipped` carries the stems,
/// and every surface built from a `Store` says so (the `MEMORY.md` index, the
/// scope listing, `search`). The one deliberate exception is [`recall`], which
/// is injected into every turn and is not a place to spend tokens on
/// maintenance chatter.
struct Store {
    memories: Vec<(String, Memory)>,
    /// Stems of files skipped because they could not be read or parsed, in
    /// directory order.
    skipped: Vec<String>,
}

impl Store {
    /// The one-line report of what this load skipped, empty when nothing was —
    /// so a caller can append it unconditionally.
    fn skipped_note(&self) -> String {
        if self.skipped.is_empty() {
            return String::new();
        }
        format!(
            "({} memory file{} skipped — not readable as a memory: {}. Run `memory` view with \
             one of those names for the parser's message.)\n",
            self.skipped.len(),
            if self.skipped.len() == 1 { "" } else { "s" },
            self.skipped.join(", ")
        )
    }
}

/// Load every memory in the scope (stem + parsed frontmatter), skipping the
/// generated index files, and reporting which files could not be loaded.
fn load_memories(root: &Path) -> Store {
    let mut store = Store {
        memories: Vec::new(),
        skipped: Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return store;
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
            store.memories.push((stem, mem));
            continue;
        }
        // A file that cannot be read, or whose frontmatter does not parse, is
        // skipped and NAMED — never cached (there is no `Memory` to cache) and
        // never substituted with an empty one, which would reach the index as a
        // blank pointer and read as a memory that had lost its contents.
        let Ok(mem) = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|content| parse_memory(&content, &stem))
        else {
            store.skipped.push(stem);
            continue;
        };
        if cacheable && let Some(mtime) = mtime {
            memory_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_path_buf())
                .or_default()
                .insert(stem.clone(), (mtime, mem.clone()));
        }
        store.memories.push((stem, mem));
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
    store
}

/// Collapse a frontmatter value to a single line of single-spaced text, for the
/// renderers that put one memory on one line (the `MEMORY.md` pointer index,
/// `search` hits, the recall header).
///
/// A `description` may legitimately contain newlines, and a raw one would split
/// a pointer across two lines — corrupting the index the way the truncating
/// parser this replaced used to corrupt the value. Flattening happens HERE, at
/// the render, never at parse time: the stored value keeps its newlines, so
/// nothing is lost on the way back to disk.
fn flatten_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
    let store = load_memories(root);
    let mut out = String::from(
        "# Memory\n\n<!-- Generated by the `memory` tool — edit the memory files, not this index. -->\n",
    );
    for ty in TYPE_ORDER {
        let mut group: Vec<&(String, Memory)> = store
            .memories
            .iter()
            .filter(|(_, m)| m.mem_type == ty)
            .collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        out.push_str(&format!("\n## {}\n", ty.as_str()));
        for (stem, mem) in group {
            // One pointer per memory: both fields are flattened, because either
            // can hold a newline once the frontmatter is real YAML.
            out.push_str(&format!(
                "- [{}]({}.md) — {}\n",
                flatten_line(&mem.name),
                stem,
                flatten_line(&mem.description)
            ));
        }
    }
    // The index is what a session loads at start, so a file the load could not
    // read is named HERE or nowhere — a memory that quietly stopped existing is
    // worse than one that says it needs fixing.
    if !store.skipped.is_empty() {
        out.push_str("\n## unreadable\n\n");
        out.push_str(&store.skipped_note());
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
    let store = load_memories(root);
    if store.memories.is_empty() && store.skipped.is_empty() {
        return format!("(no {scope} memory yet — save some with `memory` write)");
    }
    let mut names: Vec<&str> = store
        .memories
        .iter()
        .map(|(stem, _)| stem.as_str())
        .collect();
    names.sort_unstable();
    let mut out = format!("{scope} memory ({}):\n", root.display());
    for name in names {
        out.push_str(&format!("- {name}.md\n"));
    }
    // This listing is computed live, unlike the generated index, so it reports a
    // file broken since the last mutation rebuilt `MEMORY.md`.
    out.push_str(&store.skipped_note());
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
    let store = load_memories(root);
    // A skipped file was not searched, so "(no matches)" would be a claim this
    // load cannot make. Reported here as well as in the index because search
    // reads the directory live: it sees a file broken since the last rebuild.
    let skipped = store.skipped_note();
    let mut hits: Vec<(i32, String, String, String)> = Vec::new(); // (score, name, description, stem)
    for (stem, mem) in store.memories {
        let score = relevance_score(&mem, &q);
        if score > 0 {
            hits.push((score, mem.name, mem.description, stem));
        }
    }
    if hits.is_empty() {
        return format!("(no matches)\n{skipped}");
    }
    // Best first; ties broken by name for a stable order.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = String::new();
    for (_, name, description, stem) in hits {
        // One hit per line, so both fields are flattened (see `flatten_line`).
        out.push_str(&format!(
            "- {} — {} — {stem}.md\n",
            flatten_line(&name),
            flatten_line(&description)
        ));
    }
    out.push_str(&skipped);
    out
}

/// The one-line prefix that opens an injected recall block, so both the model
/// and readers can tell where recalled memory begins.
const RECALL_HEADER: &str = "[relevant memory]\n";

/// Format one recalled memory for injection: its `name` + `description` header
/// followed by the full body, then a blank-line separator.
fn format_recall_entry(mem: &Memory) -> String {
    // The header is one line (the body below it is not), so both fields are
    // flattened — see `flatten_line`.
    let mut s = format!("## {}", flatten_line(&mem.name));
    let desc = flatten_line(&mem.description);
    if !desc.is_empty() {
        s.push_str(" — ");
        s.push_str(&desc);
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
    // generated index files. Its report of unreadable files is deliberately
    // dropped here and nowhere else: this block is injected into every turn, and
    // a maintenance notice on that path costs tokens on turns that did not ask
    // for it. The index, the scope listing and `search` all carry it.
    let mut hits: Vec<(i32, Memory)> = Vec::new();
    for root in [project, global].into_iter().flatten() {
        for (_, mem) in load_memories(root).memories {
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
        let store = load_memories(&proj);
        assert_eq!(
            store.memories.len(),
            1,
            "the backup must not be loaded as a memory"
        );
        assert_eq!(store.memories[0].0, "deploy");
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);

        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(index.contains("deploy.md"), "{index}");
        assert!(!index.contains(".bak"), "{index}");
    }

    /// Two drifts in the same second keep both backups. The whole-second
    /// timestamp is the collision, so the name claim is pinned directly with a
    /// fixed one — asserting through the clock would only exercise it on runs
    /// where both edits happened to land in the same second.
    #[test]
    fn a_second_backup_in_the_same_second_does_not_replace_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deploy.md");
        std::fs::write(&file, "first").unwrap();

        let first = claim_backup_name(&file, "deploy", 1_700_000_000).unwrap();
        std::fs::copy(&file, file.with_file_name(&first)).unwrap();
        std::fs::write(&file, "second").unwrap();
        let second = claim_backup_name(&file, "deploy", 1_700_000_000).unwrap();
        std::fs::copy(&file, file.with_file_name(&second)).unwrap();

        assert_eq!(first, "deploy.1700000000.bak");
        assert_eq!(second, "deploy.1700000000-1.bak");
        // Both extensions are `bak`, so neither is loaded as a memory.
        assert!(second.ends_with(".bak"), "{second}");
        assert_eq!(
            std::fs::read_to_string(file.with_file_name(&first)).unwrap(),
            "first",
            "the earlier backup must survive the later one"
        );
        assert_eq!(
            std::fs::read_to_string(file.with_file_name(&second)).unwrap(),
            "second"
        );
    }

    /// The end-to-end shape of the same thing: two drifted edits leave two
    /// backups, each holding the content it preserved.
    #[tokio::test]
    async fn two_drifted_edits_leave_two_backups() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({"action": "write", "name": "deploy", "description": "original"}),
            &ctx,
        )
        .await
        .unwrap();

        let file = proj.join("deploy.md");
        for marker in ["# first hand edit", "# second hand edit"] {
            // The emitter always ends the file with a newline, so a marker
            // appended without one breaks the round-trip the guard checks —
            // twice over, unlike a marker that leaves the file well-formed.
            let mut hand_edited = std::fs::read_to_string(&file).unwrap();
            hand_edited.push_str(marker);
            std::fs::write(&file, &hand_edited).unwrap();
            tool.execute(
                json!({"action": "edit", "name": "deploy", "description": marker}),
                &ctx,
            )
            .await
            .unwrap();
        }

        let backups = backup_paths(&proj);
        assert_eq!(backups.len(), 2, "each drift keeps its own backup");
        let saved: Vec<String> = backups
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert!(
            saved.iter().any(|s| s.contains("description: original")),
            "the first drift's content must not have been overwritten: {saved:?}"
        );
        assert!(
            saved.iter().any(|s| s.contains("# second hand edit")),
            "{saved:?}"
        );
    }

    /// Windows reserves a handful of stems as device names, with or without an
    /// extension. They are refused everywhere: the alternative is a memory that
    /// writes on Unix and fails on Windows with an error naming neither.
    #[tokio::test]
    async fn windows_device_names_are_refused_as_memory_names() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        for name in ["con", "CON", "con.md", "aux", "NUL", "com1", "lpt9"] {
            let r = tool
                .execute(
                    json!({"action": "write", "name": name, "description": "d"}),
                    &ctx,
                )
                .await;
            // `con.md` slugs to `con-md`, which is a fine file name — the rest
            // slug to the reserved stem itself.
            if name == "con.md" {
                assert!(r.is_ok(), "'{name}' slugs to con-md, which is usable");
                continue;
            }
            let err = match r {
                Err(e) => format!("{e}"),
                Ok(ok) => panic!("'{name}' must be refused, got: {ok}"),
            };
            assert!(err.contains("device name"), "'{name}': {err}");
        }

        // Names that merely start with a reserved word are not reserved.
        for name in ["console", "com10", "auxiliary"] {
            tool.execute(
                json!({"action": "write", "name": name, "description": "d"}),
                &ctx,
            )
            .await
            .unwrap_or_else(|e| panic!("'{name}' is not a device name: {e}"));
        }
    }

    /// `index` and `memory` are the two exact stems the loader skips (`index.md`
    /// and the generated `MEMORY.md` pointer index). A `write` with either must
    /// refuse loudly — the alternative is a memory that reports "saved" and then
    /// never lists in the index, `search` or `recall` (or, on a case-insensitive
    /// filesystem, stomps the generated `MEMORY.md` itself). Case is folded by
    /// slugification, so every spelling of the stem is caught.
    #[tokio::test]
    async fn reserved_index_and_memory_stems_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        for name in ["index", "INDEX", "Index", "memory", "MEMORY", "Memory"] {
            let r = tool
                .execute(
                    json!({"action": "write", "name": name, "description": "d"}),
                    &ctx,
                )
                .await;
            let err = match r {
                Err(e) => format!("{e}"),
                Ok(ok) => panic!("'{name}' must be refused, got: {ok}"),
            };
            assert!(
                err.contains("reserved for the generated"),
                "'{name}': {err}"
            );
        }

        // Close spellings are not reserved — only the exact stems the loader
        // skips.
        for name in ["indexed", "memory-map", "memories"] {
            tool.execute(
                json!({"action": "write", "name": name, "description": "d"}),
                &ctx,
            )
            .await
            .unwrap_or_else(|e| panic!("'{name}' is not a reserved stem: {e}"));
        }
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

        let first = load_memories(&proj).memories;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "deploy");
        assert_eq!(first[0].1.body.trim(), "step one");

        // An unchanged second load is served from the cache (same content).
        let second = load_memories(&proj).memories;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1.body.trim(), "step one");

        // A content edit bumps the file's mtime → cache miss → fresh parse, so
        // the cache never serves stale data after a write.
        seed(&proj, "deploy", "how to deploy", "step two");
        let third = load_memories(&proj).memories;
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
        assert_eq!(load_memories(&proj).memories[0].1.body.trim(), "step one");

        // A same-length edit with the mtime pinned back to the first write's
        // value — indistinguishable by an mtime-only key, and the exact shape
        // `rebuild_index_reads_a_same_tick_rewrite` exercises for the index.
        let file = proj.join("deploy.md");
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        seed(&proj, "deploy", "how to deploy", "step two");
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();

        assert_eq!(
            load_memories(&proj).memories[0].1.body.trim(),
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
        let store = load_memories(&proj);
        assert!(
            store.memories.is_empty() && store.skipped.is_empty(),
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

    /// Two descriptions copied verbatim out of a live memory store. Both carry
    /// an unquoted `: ` mid-value, which is a YAML scanner error, and both were
    /// written before the emitter learned to quote — so they are exactly the
    /// input the lenient fallback exists for.
    const UNQUOTED_COLON_DESCRIPTIONS: [&str; 2] = [
        "sqeel repo: backlog lives at ./backlog.md (not docs/); 11 PTY e2e tests always fail in this sandbox",
        "Vulkan MoE seam findings 2026-08-07: idm GEMV no-ops below in_f=32; moe_topk biases logits not probs",
    ];

    /// The reason strictness has teeth, pinned by a test rather than by a comment:
    /// `serde_yaml_ng` — the library that decides — really does reject the
    /// unquoted form, and really does accept the quoted one the emitter writes.
    #[test]
    fn serde_yaml_ng_rejects_the_unquoted_colon_form() {
        for description in UNQUOTED_COLON_DESCRIPTIONS {
            let fm = format!("name: note\ndescription: {description}\ntype: project");
            let err = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&fm)
                .err()
                .unwrap_or_else(|| panic!("expected a YAML scanner error for: {fm}"));
            assert!(
                err.to_string().contains("mapping values are not allowed"),
                "unexpected error for {description:?}: {err}"
            );

            // The same value, quoted the way `emit_memory` writes it, parses —
            // so the rejection above is about the quoting, not the content.
            let mem = Memory {
                name: "note".to_string(),
                description: description.to_string(),
                mem_type: MemType::Project,
                body: String::new(),
            };
            let emitted = emit_memory(&mem);
            let inner = emitted
                .trim_start_matches("---\n")
                .trim_end_matches("---\n");
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(inner)
                .unwrap_or_else(|e| panic!("emitted frontmatter must be valid YAML: {e}\n{inner}"));
        }
    }

    /// Frontmatter YAML rejects is an ERROR carrying the parser's own complaint,
    /// never a memory salvaged into something the author did not write. The
    /// line/column must point into the FILE — the frontmatter starts on file
    /// line 2, so the failing `description` line is line 3 — or the user cannot
    /// act on it.
    #[test]
    fn a_description_yaml_rejects_is_an_error_locating_the_line() {
        for description in UNQUOTED_COLON_DESCRIPTIONS {
            let content = format!(
                "---\nname: note\ndescription: {description}\ntype: project\n---\n\nthe body\n"
            );
            let err = parse_memory(&content, "note")
                .expect_err("unparsable frontmatter must not read as a memory");
            assert!(err.contains("not valid YAML"), "{err}");
            assert!(err.contains("mapping values are not allowed"), "{err}");
            // Line 3 of the file is the `description:` line; the column is where
            // the unquoted second colon sits (1-based).
            const KEY: &str = "description: ";
            let bad_colon = KEY.len()
                + description
                    .find(": ")
                    .expect("the fixtures carry a `: ` mid-value");
            assert!(
                err.contains(&format!("at line 3 column {}", bad_colon + 1)),
                "the location must point at the file's own line/column: {err}"
            );
        }
    }

    /// A frontmatter block that opens and never closes is malformed too — it
    /// used to fall through to the no-frontmatter path, where the `---` itself
    /// became the inferred description.
    #[test]
    fn an_unterminated_fence_is_reported_not_inferred() {
        let content = "---\nname: note\ndescription: a note\n\nthe body\n";
        let err = parse_memory(content, "note").expect_err("an unclosed fence is malformed");
        assert!(err.contains("never closes"), "{err}");
    }

    /// The no-frontmatter form is a supported input, not malformed YAML: it
    /// parses, and its first non-empty line becomes the description. Strictness
    /// must not swallow it.
    #[test]
    fn a_file_with_no_frontmatter_still_parses() {
        let mem = parse_memory("# Old note\nThe deploy key lives in Vault.\n", "legacy")
            .expect("a file with no `---` block is not malformed");
        assert_eq!(mem.description, "Old note");
        assert_eq!(mem.mem_type.as_str(), "reference");
        assert_eq!(mem.name, "legacy");
        assert!(mem.body.contains("Vault"));
    }

    /// Frontmatter that parses but is not a mapping cannot carry the fields, so
    /// it is malformed rather than an empty memory.
    #[test]
    fn non_mapping_frontmatter_is_reported() {
        let err = parse_memory("---\n- one\n- two\n---\nbody\n", "listy")
            .expect_err("a YAML list is not frontmatter");
        assert!(err.contains("must be a YAML mapping"), "{err}");
        assert!(err.contains("a list"), "{err}");

        // An EMPTY block claims nothing, so it is accepted with no fields set.
        let mem = parse_memory("---\n---\nbody\n", "blank").expect("an empty block is not a lie");
        assert_eq!(mem.name, "blank");
        assert_eq!(mem.description, "");
        assert_eq!(mem.mem_type.as_str(), "reference");
    }

    /// Write a file with frontmatter YAML rejects directly into `root`, the way
    /// a hand edit or an older hrdr would have left it.
    fn seed_unparsable(root: &Path, stem: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(format!("{stem}.md")),
            format!(
                "---\nname: {stem}\ndescription: {}\ntype: project\n---\n\nnotes\n",
                UNQUOTED_COLON_DESCRIPTIONS[0]
            ),
        )
        .unwrap();
    }

    /// `view` on an unparsable file reports the file and the parser's message,
    /// instead of handing back a memory nobody wrote.
    #[tokio::test]
    async fn view_on_an_unparsable_file_errors_with_the_parser_message() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        seed_unparsable(&dir.path().join("project"), "handwritten");

        let err = tool
            .execute(json!({"action": "view", "name": "handwritten"}), &ctx)
            .await
            .expect_err("view must not return an unreadable memory");
        let err = format!("{err}");
        assert!(err.contains("handwritten.md"), "names the file: {err}");
        assert!(err.contains("mapping values are not allowed"), "{err}");
        assert!(err.contains("at line 3 column"), "{err}");
    }

    /// `edit` refuses the same file — an edit re-emits the whole memory, so
    /// editing one field of a file the parser could not read would drop the
    /// rest. And it must leave no `.bak` behind for a rewrite that never ran.
    #[tokio::test]
    async fn edit_on_an_unparsable_file_errors_and_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");
        seed_unparsable(&proj, "handwritten");
        let before = std::fs::read_to_string(proj.join("handwritten.md")).unwrap();

        let err = tool
            .execute(
                json!({"action": "edit", "name": "handwritten", "body": "replaced"}),
                &ctx,
            )
            .await
            .expect_err("edit must not rewrite a file it cannot read");
        let err = format!("{err}");
        assert!(err.contains("handwritten.md"), "{err}");
        assert!(err.contains("not valid YAML"), "{err}");

        assert_eq!(
            std::fs::read_to_string(proj.join("handwritten.md")).unwrap(),
            before,
            "a refused edit must not touch the file"
        );
        assert!(
            backup_paths(&proj).is_empty(),
            "a refused edit must not leave a backup: {:?}",
            backup_paths(&proj)
        );
    }

    /// `write` is create-or-replace, so it still succeeds over an unparsable
    /// file — and because such content cannot round-trip, the drift guard must
    /// preserve it first. That backup is the whole reason `write` may proceed
    /// where `view` and `edit` refuse.
    #[tokio::test]
    async fn write_over_an_unparsable_file_succeeds_and_backs_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");
        seed_unparsable(&proj, "handwritten");

        let out = tool
            .execute(
                json!({"action": "write", "name": "handwritten", "description": "rewritten cleanly"}),
                &ctx,
            )
            .await
            .expect("write is create-or-replace");
        assert!(out.contains("preserved a hand-edited file as"), "{out}");

        // The replacement parses…
        let raw = std::fs::read_to_string(proj.join("handwritten.md")).unwrap();
        assert_eq!(
            parse_memory(&raw, "handwritten").unwrap().description,
            "rewritten cleanly"
        );
        // …and the unparsable original survives in the backup.
        let backups = backup_paths(&proj);
        assert_eq!(backups.len(), 1, "{backups:?}");
        let bak = std::fs::read_to_string(&backups[0]).unwrap();
        assert!(bak.contains(UNQUOTED_COLON_DESCRIPTIONS[0]), "{bak}");
    }

    /// The store keeps working around a broken file — but the skip is reported
    /// everywhere a user reads the store: the generated index, the live scope
    /// listing, and `search`. A silently missing memory is the failure mode this
    /// whole shape exists to prevent.
    #[tokio::test]
    async fn a_skipped_file_is_named_in_the_index_the_listing_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");
        seed_unparsable(&proj, "handwritten");
        seed_unparsable(&proj, "also-broken");

        let store = load_memories(&proj);
        assert!(store.memories.is_empty(), "neither file can be loaded");
        assert_eq!(store.skipped.len(), 2, "{:?}", store.skipped);

        // The live listing (view with no name, before any index exists).
        let listing = tool.execute(json!({"action": "view"}), &ctx).await.unwrap();
        assert!(listing.contains("2 memory files skipped"), "{listing}");
        assert!(listing.contains("handwritten"), "{listing}");
        assert!(listing.contains("also-broken"), "{listing}");

        // `search` reads the directory live, and must not claim "(no matches)"
        // over files it never searched.
        let hits = tool
            .execute(json!({"action": "search", "query": "sqeel"}), &ctx)
            .await
            .unwrap();
        assert!(hits.contains("2 memory files skipped"), "{hits}");
        assert!(hits.contains("handwritten"), "{hits}");

        // And the generated index, which is what a session loads at start.
        tool.execute(
            json!({"action": "write", "name": "seed", "description": "seed"}),
            &ctx,
        )
        .await
        .unwrap();
        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(index.contains("## unreadable"), "{index}");
        assert!(index.contains("2 memory files skipped"), "{index}");
        assert!(index.contains("handwritten"), "{index}");
        assert!(index.contains("also-broken"), "{index}");
        // The working memory is still listed — one broken file must not take the
        // scope down.
        assert!(index.contains("- [seed](seed.md)"), "{index}");
    }

    /// The reverse: a healthy store says nothing about skipped files. A report
    /// that is always printed is not a report.
    #[tokio::test]
    async fn a_healthy_store_reports_no_skipped_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({"action": "write", "name": "seed", "description": "seed"}),
            &ctx,
        )
        .await
        .unwrap();
        // A frontmatter-less legacy file is NOT a skipped file.
        std::fs::write(proj.join("legacy.md"), "# Old note\nbody\n").unwrap();

        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        assert!(!index.contains("skipped"), "{index}");
        let hits = tool
            .execute(json!({"action": "search", "query": "seed"}), &ctx)
            .await
            .unwrap();
        assert!(!hits.contains("skipped"), "{hits}");
        assert!(load_memories(&proj).skipped.is_empty());
    }

    /// Recall is injected into every turn, so it stays quiet about maintenance:
    /// a broken file must neither break recall nor add a line to it.
    #[test]
    fn recall_says_nothing_about_skipped_files() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "how to deploy", "Run the deploy script.");
        seed_unparsable(&proj, "handwritten");

        let block = recall(Some(&proj), None, "how do I deploy this", 4096).unwrap();
        assert!(block.contains("Run the deploy script."), "{block}");
        assert!(!block.contains("skipped"), "{block}");
        assert!(!block.contains("handwritten"), "{block}");
    }

    /// The bug this fix removes: a `description` with a newline was written
    /// across two lines and read back as its first line only. It must now
    /// survive a `write`, an `edit` and the reload in between — and reach the
    /// index as ONE pointer line, flattened.
    #[tokio::test]
    async fn a_multiline_description_survives_write_edit_and_the_index() {
        const DESCRIPTION: &str = "first line of the description\nsecond line that used to vanish";
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({"action": "write", "name": "multi", "description": DESCRIPTION, "body": "body one"}),
            &ctx,
        )
        .await
        .unwrap();
        // An edit that does not touch the description re-emits it from the
        // parsed file — the step that used to persist the truncation.
        tool.execute(
            json!({"action": "edit", "name": "multi", "body": "body two"}),
            &ctx,
        )
        .await
        .unwrap();

        let raw = std::fs::read_to_string(proj.join("multi.md")).unwrap();
        let mem = parse_memory(&raw, "multi").unwrap();
        assert_eq!(mem.description, DESCRIPTION, "{raw}");
        assert_eq!(mem.body.trim(), "body two", "{raw}");

        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        let pointers: Vec<&str> = index.lines().filter(|l| l.contains("multi.md")).collect();
        assert_eq!(pointers.len(), 1, "one pointer line per memory: {index}");
        assert!(
            pointers[0].contains("first line of the description second line that used to vanish"),
            "the pointer must carry the whole description, flattened: {index}"
        );

        // `search` is line-oriented too.
        let hits = tool
            .execute(json!({"action": "search", "query": "used to vanish"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            hits.lines().filter(|l| l.contains("multi.md")).count(),
            1,
            "{hits}"
        );
    }

    /// A multi-line description round-trips, so the drift guard has nothing to
    /// preserve: repeated edits must not spray `.bak` files. Before the YAML
    /// emitter this failed on the FIRST edit — the file could never round-trip.
    #[tokio::test]
    async fn a_multiline_description_does_not_churn_backups() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({
                "action": "write",
                "name": "multi",
                "description": "first line\nsecond line",
                "body": "body one"
            }),
            &ctx,
        )
        .await
        .unwrap();

        for (n, body) in ["body two", "body three"].iter().enumerate() {
            let out = tool
                .execute(
                    json!({"action": "edit", "name": "multi", "body": body}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(
                !out.contains("preserved a hand-edited file"),
                "edit {n}: {out}"
            );
            assert!(
                backup_paths(&proj).is_empty(),
                "edit {n} made a backup: {:?}",
                backup_paths(&proj)
            );
        }
    }

    /// A description that IS a quoted string — the quote characters are part of
    /// the value. The hand-rolled reader used to strip them, so the value could
    /// never round-trip.
    #[tokio::test]
    async fn a_quoted_string_description_round_trips() {
        const DESCRIPTION: &str = "\"ship it\"";
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({"action": "write", "name": "slogan", "description": DESCRIPTION}),
            &ctx,
        )
        .await
        .unwrap();
        tool.execute(
            json!({"action": "edit", "name": "slogan", "body": "added later"}),
            &ctx,
        )
        .await
        .unwrap();

        let raw = std::fs::read_to_string(proj.join("slogan.md")).unwrap();
        assert_eq!(
            parse_memory(&raw, "slogan").unwrap().description,
            DESCRIPTION,
            "{raw}"
        );
        assert!(backup_paths(&proj).is_empty(), "the value round-trips");
    }

    /// The body is not frontmatter: lines that look like a fence or a mapping
    /// entry are content, and survive a write and an edit untouched.
    #[tokio::test]
    async fn a_body_of_frontmatter_looking_lines_is_untouched() {
        const BODY: &str = "---\nkey: value\n---\n\nnot frontmatter: really";
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        tool.execute(
            json!({
                "action": "write",
                "name": "tricky",
                "description": "a body full of fences",
                "body": BODY
            }),
            &ctx,
        )
        .await
        .unwrap();
        tool.execute(
            json!({"action": "edit", "name": "tricky", "description": "still fine"}),
            &ctx,
        )
        .await
        .unwrap();

        let raw = std::fs::read_to_string(proj.join("tricky.md")).unwrap();
        let mem = parse_memory(&raw, "tricky").unwrap();
        assert_eq!(mem.body.trim(), BODY, "{raw}");
        assert_eq!(mem.description, "still fine", "{raw}");
        assert!(backup_paths(&proj).is_empty(), "the file round-trips");
    }

    /// Awkward values a `description` may now legally hold, each written and
    /// read back unchanged — and emitting the parse of an emitted file must
    /// reproduce it byte for byte, which is the invariant `backup_if_drifted`
    /// tests for drift with.
    #[test]
    fn emit_and_parse_round_trip_awkward_descriptions() {
        for description in [
            UNQUOTED_COLON_DESCRIPTIONS[0],
            "line one\nline two",
            // A fence line inside the value: emitted indented inside a block
            // scalar, so the closing-fence scan must not stop on it.
            "before\n---\nafter",
            "\"ship it\"",
            "'single'",
            "",
            "- leading dash",
            "#leading hash",
            "%leading percent",
            "trailing space ",
            "  leading space",
            "key: value",
        ] {
            let mem = Memory {
                name: "round".to_string(),
                description: description.to_string(),
                mem_type: MemType::Feedback,
                body: "the body".to_string(),
            };
            let content = emit_memory(&mem);
            let back = parse_memory(&content, "round")
                .unwrap_or_else(|e| panic!("an emitted file must parse: {e}\n{content}"));
            assert_eq!(back.description, description, "in:\n{content}");
            assert_eq!(back.name, "round", "in:\n{content}");
            assert_eq!(back.mem_type.as_str(), "feedback", "in:\n{content}");
            assert_eq!(back.body.trim(), "the body", "in:\n{content}");
            assert_eq!(
                emit_memory(&back),
                content,
                "emit(parse(x)) must reproduce x for {description:?}"
            );
        }
    }

    /// The argument-shaped refusals: an action or scope the tool does not have.
    /// Both are `bail!` arms that no other test reaches, and both must name what
    /// was wrong rather than doing something arbitrary.
    #[tokio::test]
    async fn unknown_action_and_unknown_scope_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        let err = tool
            .execute(json!({"action": "forget", "name": "x"}), &ctx)
            .await
            .expect_err("'forget' is not an action");
        assert!(format!("{err}").contains("unknown memory action"), "{err}");

        let err = tool
            .execute(json!({"action": "view", "scope": "team"}), &ctx)
            .await
            .expect_err("'team' is not a scope");
        let err = format!("{err}");
        assert!(err.contains("unknown memory scope"), "{err}");
        assert!(
            err.contains("project"),
            "the error must name the valid scopes: {err}"
        );
    }

    #[tokio::test]
    async fn delete_on_a_missing_memory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let err = tool
            .execute(json!({"action": "delete", "name": "never-existed"}), &ctx)
            .await
            .expect_err("deleting nothing is not a success");
        assert!(format!("{err}").contains("never-existed"), "{err}");
    }

    /// Every declared `type` is accepted and lands in the file, an undeclared one
    /// is refused with the list, and the index groups the results in
    /// [`TYPE_ORDER`] — the order the model reads them in at session start.
    #[tokio::test]
    async fn every_type_writes_and_the_index_groups_them_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        // Written in the reverse of TYPE_ORDER, so an index that merely echoed
        // insertion order would come out backwards.
        for ty in TYPE_ORDER.iter().rev() {
            tool.execute(
                json!({"action": "write", "name": ty.as_str(), "type": ty.as_str(), "description": "d"}),
                &ctx,
            )
            .await
            .unwrap_or_else(|e| panic!("type '{}' must be accepted: {e}", ty.as_str()));
            let raw = std::fs::read_to_string(proj.join(format!("{}.md", ty.as_str()))).unwrap();
            assert_eq!(parse_memory(&raw, ty.as_str()).unwrap().mem_type, *ty);
        }

        let index = std::fs::read_to_string(proj.join("MEMORY.md")).unwrap();
        let positions: Vec<usize> = TYPE_ORDER
            .iter()
            .map(|ty| {
                index
                    .find(&format!("## {}", ty.as_str()))
                    .unwrap_or_else(|| panic!("no '## {}' section in:\n{index}", ty.as_str()))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "sections must follow TYPE_ORDER, got {positions:?} in:\n{index}"
        );

        // A type outside the set is a typo, not a new category.
        let err = tool
            .execute(
                json!({"action": "write", "name": "odd", "type": "urgent", "description": "d"}),
                &ctx,
            )
            .await
            .expect_err("'urgent' is not a memory type");
        let err = format!("{err}");
        assert!(err.contains("unknown memory type 'urgent'"), "{err}");
        assert!(
            err.contains("feedback"),
            "the error must list the types: {err}"
        );
        assert!(
            !proj.join("odd.md").exists(),
            "a refused write must not land"
        );
    }

    /// `edit` updates only what it was given. Each field is checked with the
    /// OTHER two left out, so a bug that reset a field to its default on any
    /// single-field edit is caught wherever it lives.
    #[tokio::test]
    async fn edit_preserves_every_field_it_was_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;
        let proj = dir.path().join("project");

        let read = |stem: &str| {
            let raw = std::fs::read_to_string(proj.join(format!("{stem}.md"))).unwrap();
            parse_memory(&raw, stem).unwrap()
        };
        let seed_one = async |stem: &str| {
            tool.execute(
                json!({"action": "write", "name": stem, "type": "feedback",
                       "description": "the original description", "body": "the original body"}),
                &ctx,
            )
            .await
            .unwrap();
        };

        // Body only: description and type survive.
        seed_one("body-only").await;
        tool.execute(
            json!({"action": "edit", "name": "body-only", "body": "a new body"}),
            &ctx,
        )
        .await
        .unwrap();
        let mem = read("body-only");
        assert_eq!(mem.body.trim(), "a new body");
        assert_eq!(mem.description, "the original description");
        assert_eq!(mem.mem_type, MemType::Feedback);

        // Description only: body and type survive.
        seed_one("desc-only").await;
        tool.execute(
            json!({"action": "edit", "name": "desc-only", "description": "a new description"}),
            &ctx,
        )
        .await
        .unwrap();
        let mem = read("desc-only");
        assert_eq!(mem.description, "a new description");
        assert_eq!(mem.body.trim(), "the original body");
        assert_eq!(mem.mem_type, MemType::Feedback);

        // Type only: description and body survive.
        seed_one("type-only").await;
        tool.execute(
            json!({"action": "edit", "name": "type-only", "type": "project"}),
            &ctx,
        )
        .await
        .unwrap();
        let mem = read("type-only");
        assert_eq!(mem.mem_type, MemType::Project);
        assert_eq!(mem.description, "the original description");
        assert_eq!(mem.body.trim(), "the original body");
    }

    /// One name, two scopes, two independent memories: neither store's write
    /// reaches the other's file, index or search.
    #[tokio::test]
    async fn the_same_name_in_both_scopes_does_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        for (scope, description) in [("project", "the project one"), ("global", "the global one")] {
            tool.execute(
                json!({"action": "write", "scope": scope, "name": "notes", "description": description}),
                &ctx,
            )
            .await
            .unwrap();
        }

        for (scope, mine, theirs) in [
            ("project", "the project one", "the global one"),
            ("global", "the global one", "the project one"),
        ] {
            let raw = std::fs::read_to_string(dir.path().join(scope).join("notes.md")).unwrap();
            assert_eq!(parse_memory(&raw, "notes").unwrap().description, mine);

            let view = tool
                .execute(json!({"action": "view", "scope": scope}), &ctx)
                .await
                .unwrap();
            assert!(view.contains(mine), "{scope}: {view}");
            assert!(
                !view.contains(theirs),
                "{scope} leaked the other scope: {view}"
            );
        }

        // Deleting one leaves the other in place.
        tool.execute(
            json!({"action": "delete", "scope": "project", "name": "notes"}),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!dir.path().join("project").join("notes.md").exists());
        assert!(dir.path().join("global").join("notes.md").exists());
    }

    /// Names that cannot become a file: nothing survives slugification, or the
    /// name is a Windows-style path. A refusal is required — the alternative is
    /// a memory saved under a name the caller cannot ask for again.
    #[tokio::test]
    async fn a_name_with_no_usable_slug_or_a_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        for name in ["!!!", "...", "   ", "—", "\\", "notes\\deploy"] {
            let err = match tool
                .execute(
                    json!({"action": "write", "name": name, "description": "d"}),
                    &ctx,
                )
                .await
            {
                Err(e) => format!("{e}"),
                Ok(out) => panic!("'{name}' must be refused, got: {out}"),
            };
            assert!(
                err.contains("slug") || err.contains("path") || err.contains("empty"),
                "'{name}': {err}"
            );
        }
        // Nothing was created by any of them.
        let proj = dir.path().join("project");
        assert!(load_memories(&proj).memories.is_empty());

        // A name past the slug limit is refused by the tool, with the limit
        // named — not by the filesystem, whose `File name too long (os error
        // 36)` names neither the tool nor the memory. (Found by this test: the
        // cap did not exist and the errno reached the caller.)
        let too_long = "a".repeat(MAX_SLUG_LEN + 1);
        let err = match tool
            .execute(
                json!({"action": "write", "name": too_long, "description": "d"}),
                &ctx,
            )
            .await
        {
            Err(e) => format!("{e}"),
            Ok(out) => panic!(
                "a {}-character slug must be refused, got: {out}",
                too_long.len()
            ),
        };
        assert!(err.contains(&MAX_SLUG_LEN.to_string()), "{err}");

        // A name AT the limit is fine, so the cap is a boundary and not a ban on
        // long names.
        let at_limit = "b".repeat(MAX_SLUG_LEN);
        tool.execute(
            json!({"action": "write", "name": at_limit, "description": "d"}),
            &ctx,
        )
        .await
        .unwrap();
        assert!(proj.join(format!("{at_limit}.md")).exists());
    }

    /// A message made only of stopwords and short words has no meaningful terms,
    /// so recall returns nothing rather than matching on noise.
    #[test]
    fn a_stopword_only_message_recalls_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "how to deploy", "Run the deploy script.");

        assert!(recall_tokens("what can you do with all of them").is_empty());
        assert!(
            recall(Some(&proj), None, "what can you do with all of them", 4096).is_none(),
            "a stopword-only message must not pull memories in"
        );
        // The same sentence with one meaningful term does recall.
        assert!(recall(Some(&proj), None, "what can you deploy", 4096).is_some());
    }

    /// `view` of one memory goes through `truncate_saved`, so an oversized file
    /// comes back bounded instead of blowing the call's output budget.
    #[tokio::test]
    async fn view_truncates_an_oversized_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_memory(dir.path());
        let tool = MemoryTool;

        let body = "line of text\n".repeat(400);
        tool.execute(
            json!({"action": "write", "name": "big", "description": "a big one", "body": body}),
            &ctx,
        )
        .await
        .unwrap();
        let raw = std::fs::read_to_string(dir.path().join("project").join("big.md")).unwrap();

        // Under a tight cap the output is bounded and says it was cut…
        ctx.max_output = 300;
        ctx.max_output_lines = 20;
        let cut = tool
            .execute(json!({"action": "view", "name": "big"}), &ctx)
            .await
            .unwrap();
        assert!(cut.len() < raw.len(), "not truncated: {} bytes", cut.len());
        assert!(cut.contains('…'), "no truncation marker: {cut}");

        // …and with room, the same call returns the file whole, so the assertion
        // above is about the cap and not about `view` always truncating.
        ctx.max_output = 1 << 20;
        ctx.max_output_lines = 10_000;
        let whole = tool
            .execute(json!({"action": "view", "name": "big"}), &ctx)
            .await
            .unwrap();
        assert_eq!(whole, raw);
    }

    /// The backup namer gives up rather than looping forever or overwriting:
    /// one second holds [`MAX_BACKUPS_PER_SECOND`] copies of one memory, and the
    /// next attempt is an error the caller turns into a refusal to overwrite.
    #[test]
    fn claim_backup_name_gives_up_at_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deploy.md");
        std::fs::write(&file, "content").unwrap();

        for n in 0..MAX_BACKUPS_PER_SECOND {
            let name = claim_backup_name(&file, "deploy", 1_700_000_000)
                .unwrap_or_else(|e| panic!("claim {n} of the bound must succeed: {e}"));
            assert!(name.ends_with(".bak"), "{name}");
        }
        let err = claim_backup_name(&file, "deploy", 1_700_000_000)
            .expect_err("the bound must stop the search");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            err.to_string().contains("deploy"),
            "the error must name the memory: {err}"
        );

        // A different second is unaffected — the bound is per timestamp.
        assert!(claim_backup_name(&file, "deploy", 1_700_000_001).is_ok());
    }

    /// A memory deleted while its parse sits in the cache must not come back.
    /// The load prunes cache entries for files it did not enumerate, so a later
    /// file of the same name — even one whose mtime matches the pruned entry, as
    /// a coarse-granularity filesystem can produce — is read fresh.
    #[test]
    fn a_deleted_memory_is_not_served_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("project");
        seed(&proj, "deploy", "how to deploy", "the old body");
        let file = proj.join("deploy.md");
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

        // Cache it, then delete it: the deleted memory is gone from the load…
        assert_eq!(load_memories(&proj).memories.len(), 1);
        std::fs::remove_file(&file).unwrap();
        assert!(load_memories(&proj).memories.is_empty(), "deleted, so gone");

        // …and a new file at the same name and mtime reads as its own content,
        // not as the entry the deleted file left behind.
        seed(&proj, "deploy", "how to deploy", "the new body");
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(mtime)).unwrap();
        let store = load_memories(&proj);
        assert_eq!(store.memories.len(), 1);
        assert_eq!(
            store.memories[0].1.body.trim(),
            "the new body",
            "a stale cache entry outlived the file it was parsed from"
        );
    }
}
