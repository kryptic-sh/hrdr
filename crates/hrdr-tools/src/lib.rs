//! `hrdr-tools` — the agentic tool set.
//!
//! The built-in set: `read`, `write`, `edit`, `patch`, `bash`, `grep`, `find`, `ls`,
//! `todo`, `fetch`, `search`. Each implements [`Tool`] and is exposed to the model
//! as a native OpenAI function. Efficiency is in the design: token-bounded
//! outputs, line-numbered reads for precise edits, ripgrep-backed search.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use hrdr_llm::ToolDef;

mod checkpoint;
mod guardrails;
mod hooks;
mod mcp;
mod memory;
mod patch;
mod tools;
mod web;

pub use checkpoint::{CheckpointInfo, Checkpoints};
pub use guardrails::{Guardrail, check_guardrails, default_guardrails};
pub use hooks::{DEFAULT_HOOK_TIMEOUT_MS, Hook, run_file_hooks};
pub use mcp::McpClient;
pub use memory::MemoryTool;
pub use patch::PatchTool;
pub use tools::{
    BashTool, EditTool, FindTool, GrepTool, LsTool, PowerShellTool, ReadTool, TodoTool, WriteTool,
    available_shell_tools,
};
pub use web::{WebFetchTool, WebSearchTool};

/// Default cap on a single tool's textual output, in bytes. Larger results are
/// truncated (and, for `bash`/`grep`, saved to disk) so the model's context is
/// never blown by one call. Matches opencode's `tool_output.max_bytes`.
pub const DEFAULT_MAX_OUTPUT: usize = 51_200;

/// Default cap on a single tool's output in *lines*, applied alongside
/// [`DEFAULT_MAX_OUTPUT`] by [`truncate_saved`] (whichever limit is hit first).
/// Matches opencode's `tool_output.max_lines`.
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2_000;

/// A single TODO item tracked by `todo`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub content: String,
    /// `pending` | `in_progress` | `completed`.
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_status() -> String {
    "pending".to_string()
}

/// A detached background sub-agent (`task` with `background: true`): it runs
/// concurrently with the main agent, streaming into `log`; when `done`, its
/// `result` is delivered into the conversation and the entry is pruned. Shared
/// via [`ToolContext::background_tasks`] so the frontend can show live progress.
#[derive(Debug, Clone)]
pub struct BackgroundTask {
    /// Stable id for the run — shown to the model and used for delivery matching.
    pub id: u64,
    /// Short label (agent/description) for the panel and delivery notice.
    pub label: String,
    /// Accumulated live output (streamed answer text + tool-activity markers).
    pub log: String,
    /// Whether the sub-agent has finished.
    pub done: bool,
    /// The final result, once `done`.
    pub result: Option<String>,
    /// Whether the result has been injected into the conversation yet.
    pub delivered: bool,
}

/// Shared execution context handed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    /// Working directory tool paths resolve against.
    pub cwd: PathBuf,
    /// Shared TODO list, mutated by `todo`, surfaced to the UI.
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
    /// Per-call output byte cap.
    pub max_output: usize,
    /// Per-call output line cap, applied alongside [`max_output`](Self::max_output)
    /// by [`truncate_saved`] (whichever is hit first).
    pub max_output_lines: usize,
    /// Optional live-output sink: long-running tools (e.g. `bash`) send partial
    /// output here as it's produced so the UI can show progress. `None` = no
    /// streaming consumer.
    pub stream: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Optional checkpoint store: file-mutating tools record a file's pre-image
    /// here before writing, so edits can be reverted. `None` = no checkpointing.
    pub checkpoints: Option<Arc<Mutex<Checkpoints>>>,
    /// Shell-command guardrails ([`default_guardrails`] plus any user rules);
    /// the shell tools reject a matching command with the rule's message.
    pub guardrails: Arc<Vec<Guardrail>>,
    /// Files whose current content the model has seen (read, or written by it
    /// this session). `edit`/`write` refuse to mutate an existing file
    /// that isn't here — blind edits against guessed content are the top
    /// source of corrupt patches.
    pub read_files: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    /// When set (the default), `write`/`edit` refuse paths outside the
    /// working directory (the system temp dir is always allowed for scratch).
    /// Disable via config `allow_outside_cwd = true`.
    pub restrict_to_cwd: bool,
    /// When set, file-mutating tools (`write`/`edit`/`patch`) may only touch
    /// files with one of these extensions (case-insensitive, no dot — e.g.
    /// `["md", "markdown"]`). `None` = any extension. Used to scope a sub-agent
    /// to writing only certain file types (e.g. a planner that persists Markdown).
    pub write_allow_ext: Option<Vec<String>>,
    /// Storage root for **project-scoped** [`MemoryTool`] notes (this cwd).
    /// `None` disables project memory.
    pub memory_project: Option<PathBuf>,
    /// Storage root for **global** [`MemoryTool`] notes (all projects).
    /// `None` disables global memory.
    pub memory_global: Option<PathBuf>,
    /// Detached background sub-agents (`task` with `background: true`), shared so
    /// the run loop can deliver their results and the frontend can show progress.
    pub background_tasks: Arc<Mutex<Vec<BackgroundTask>>>,
    /// Post-edit hooks from `[[hooks]]` config (formatters, mostly), run by
    /// `edit`/`write` after a successful mutation.
    pub hooks: Arc<Vec<Hook>>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            todos: Arc::new(Mutex::new(Vec::new())),
            max_output: DEFAULT_MAX_OUTPUT,
            max_output_lines: DEFAULT_MAX_OUTPUT_LINES,
            stream: None,
            checkpoints: None,
            guardrails: Arc::new(default_guardrails()),
            read_files: Arc::new(Mutex::new(std::collections::HashSet::new())),
            restrict_to_cwd: true,
            write_allow_ext: None,
            memory_project: None,
            memory_global: None,
            background_tasks: Arc::new(Mutex::new(Vec::new())),
            hooks: Arc::new(Vec::new()),
        }
    }

    /// Send a chunk of live output to the streaming sink, if one is attached.
    pub fn emit(&self, chunk: impl Into<String>) {
        if let Some(tx) = &self.stream {
            let _ = tx.send(chunk.into());
        }
    }

    /// Snapshot a file's current content into the checkpoint store (if any)
    /// before a tool modifies it, so the change can be reverted.
    pub fn checkpoint(&self, path: &std::path::Path) {
        if let Some(cp) = &self.checkpoints
            && let Ok(mut cp) = cp.lock()
        {
            cp.record_pre(path);
        }
    }

    /// Resolve a possibly-relative path against `cwd`.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() { p } else { self.cwd.join(p) }
    }

    /// Record that the model has seen `path`'s current content (a successful
    /// read, or a write it authored). Canonicalized so relative/absolute
    /// spellings of the same file agree.
    pub fn mark_read(&self, path: &std::path::Path) {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Ok(mut set) = self.read_files.lock() {
            set.insert(canon);
        }
    }

    /// Guard for the file-mutating tools: `Err` when `path` escapes both the
    /// working directory and the system temp dir (scratch space) while
    /// [`restrict_to_cwd`](Self::restrict_to_cwd) is on. Compares canonical
    /// paths (via the nearest existing ancestor, so not-yet-created files
    /// resolve too) — `../` tricks don't slip through.
    pub fn ensure_within_cwd(&self, path: &std::path::Path) -> Result<()> {
        self.ensure_writable_ext(path)?;
        if !self.restrict_to_cwd {
            return Ok(());
        }
        let canon = canonicalize_nearest(path);
        let cwd = canonicalize_nearest(&self.cwd);
        if canon.starts_with(&cwd) || canon.starts_with(canonicalize_nearest(&std::env::temp_dir()))
        {
            return Ok(());
        }
        Err(anyhow!(
            "{} is outside the working directory ({}) — file changes are confined to the \
             project; ask the user to change it themselves (or to set allow_outside_cwd)",
            path.display(),
            self.cwd.display()
        ))
    }

    /// Guard for [`write_allow_ext`](Self::write_allow_ext): `Err` when a
    /// mutating tool targets a file whose extension isn't in the allow-list.
    /// A no-op when no list is set.
    pub fn ensure_writable_ext(&self, path: &std::path::Path) -> Result<()> {
        let Some(allowed) = &self.write_allow_ext else {
            return Ok(());
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if allowed.iter().any(|a| a.eq_ignore_ascii_case(ext)) {
            return Ok(());
        }
        Err(anyhow!(
            "this agent may only modify {} files — {} is not allowed",
            allowed
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join("/"),
            path.display()
        ))
    }

    /// Whether the model has seen `path`'s current content this session.
    pub fn was_read(&self, path: &std::path::Path) -> bool {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.read_files
            .lock()
            .map(|set| set.contains(&canon))
            .unwrap_or(true) // poisoned lock: don't wedge edits
    }
}

/// Canonicalize `path` by resolving its nearest existing ancestor (the path
/// itself may not exist yet — e.g. a file about to be created) and re-joining
/// the non-existent remainder.
pub(crate) fn canonicalize_nearest(path: &std::path::Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canon) = existing.canonicalize() {
            let mut out = canon;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Credential/secret file patterns the content-reading tools (`read`, `grep`)
/// refuse to return. Prompt-injected content (a README, a fetched page) can
/// instruct the agent to read the credential store and smuggle the keys out via
/// a `fetch` URL; this deny-list is the mechanical guardrail that turns that
/// class of attack into a corrective tool error rather than an exfiltration.
///
/// Matching is **structural** (path components / file suffixes), not
/// home-relative, and expects an already-resolved path (see
/// [`guard_secret_read`], which canonicalizes first) so a `..`-escape or an
/// absolute spelling is caught the same way as a tilde path. Returns
/// `Some(reason)` naming the matched category, else `None`.
///
/// This is the single, well-documented pattern set — extend the arms here to
/// broaden coverage; every content-reading tool routes through it.
pub(crate) fn secret_file_reason(path: &std::path::Path) -> Option<&'static str> {
    use std::path::Component;
    let comps: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let n = comps.len();
    let file = comps.last().map(String::as_str).unwrap_or("");
    let parent = if n >= 2 { comps[n - 2].as_str() } else { "" };

    // hrdr credential store: `<config>/hrdr/auth.toml` (XDG or ~/.config).
    if parent == "hrdr" && file == "auth.toml" {
        return Some("hrdr credential store (auth.toml)");
    }
    // SSH keys/config — the whole ~/.ssh directory is off-limits.
    if comps.iter().any(|c| c == ".ssh") {
        return Some("SSH directory (~/.ssh)");
    }
    // AWS static credentials.
    if parent == ".aws" && file == "credentials" {
        return Some("AWS credentials file");
    }
    // GitHub CLI stored host tokens.
    if parent == "gh" && (file == "hosts.yml" || file == "hosts.yaml") {
        return Some("GitHub CLI host tokens (gh/hosts.yml)");
    }
    // dotenv files (.env, .env.local, .env.production, …) — but NOT the
    // non-secret template variants (.env.example/.sample/.template/.dist) that
    // coding agents legitimately read to learn which vars a project expects.
    if file == ".env"
        || (file.starts_with(".env.")
            && !matches!(
                file,
                ".env.example" | ".env.sample" | ".env.template" | ".env.dist"
            ))
    {
        return Some("environment/secrets file (.env)");
    }
    // Private key material by extension.
    if file.ends_with(".pem") || file.ends_with(".key") {
        return Some("private key file (.pem/.key)");
    }
    None
}

/// Guard a content read: canonicalize `path` (resolving symlinks and `..`) then
/// reject it with a corrective error when it names a credential/secret file per
/// [`secret_file_reason`]. Used by the `read` and `grep` tools.
pub(crate) fn guard_secret_read(path: &std::path::Path) -> Result<()> {
    let resolved = canonicalize_nearest(path);
    if let Some(reason) = secret_file_reason(&resolved) {
        return Err(anyhow!(
            "refusing to read {}: {reason} — secret/credential files are off-limits to \
             the read/grep tools; if the user genuinely needs this, they must provide it",
            path.display()
        ));
    }
    Ok(())
}

/// Whether a search-output line (`path:NN:…` or `path-NN-…`) names a secret
/// file, so the grep backends can drop it before returning. `cwd` anchors a
/// relative path token. Best-effort: only the `:`-delimited leading token is
/// inspected (covers the common no-context `grep`/`rg` match line).
pub(crate) fn grep_line_is_secret(line: &str, cwd: &std::path::Path) -> bool {
    let Some((tok, _)) = line.split_once(':') else {
        return false; // `--` separators and colon-less context lines ride along
    };
    if tok.is_empty() || tok == "--" {
        return false;
    }
    secret_file_reason(&canonicalize_nearest(&cwd.join(tok))).is_some()
}

/// A model-callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the call arguments.
    fn parameters(&self) -> serde_json::Value;

    /// Whether this tool only observes state (read/search/fetch). The agent
    /// runs consecutive read-only calls concurrently; mutating tools (the
    /// default) stay strictly sequential in call order.
    fn read_only(&self) -> bool {
        false
    }

    /// Whether consecutive calls of this tool are safe to run **concurrently**
    /// with each other (and with read-only calls). Read-only tools qualify by
    /// definition; a mutating tool whose calls are self-contained and don't need
    /// to observe each other's effects in order (e.g. `task` sub-agents, each in
    /// its own isolated context) can opt in by overriding this to `true` while
    /// staying non-`read_only`. The parent's own file-mutating tools keep the
    /// default (barrier, sequential).
    fn concurrent(&self) -> bool {
        self.read_only()
    }

    /// Run the tool. A returned `Err` is surfaced to the model as a tool
    /// result, not propagated as a hard failure — the agent keeps going.
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String>;

    fn to_def(&self) -> ToolDef {
        ToolDef::function(self.name(), self.description(), self.parameters())
    }
}

/// Ordered registry of tools, keyed by name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
    order: Vec<&'static str>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The default set: file/search/todo/web tools plus whichever shells are
    /// actually available on this machine (`bash` and/or `powershell`).
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(patch::PatchTool));
        // Shell tools are presence-gated so the model is only offered a shell it
        // can actually use (bash on unix; PowerShell where installed, incl. Linux).
        for shell in available_shell_tools() {
            r.register(shell);
        }
        r.register(Arc::new(GrepTool::detect()));
        r.register(Arc::new(FindTool));
        r.register(Arc::new(LsTool));
        r.register(Arc::new(TodoTool));
        r.register(Arc::new(WebFetchTool));
        r.register(Arc::new(WebSearchTool));
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name();
        if self.tools.insert(name, tool).is_none() {
            self.order.push(name);
        }
    }

    /// Tool definitions for the request `tools[]`, in registration order.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n))
            .map(|t| t.to_def())
            .collect()
    }

    /// Whether `name` is a registered read-only tool (see
    /// [`Tool::read_only`]); unknown names count as mutating.
    pub fn is_read_only(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.read_only())
    }

    /// Scope the registry to an allow-list of tool names (for a restricted
    /// sub-agent). Anything not in `allowed` is dropped; unknown names in
    /// `allowed` are simply ignored. Registration order is preserved.
    pub fn retain_only(&mut self, allowed: &[String]) {
        let keep = |n: &str| allowed.iter().any(|a| a == n);
        self.order.retain(|n| keep(n));
        self.tools.retain(|n, _| keep(n));
    }

    /// Names of the currently-registered read-only tools, in registration
    /// order — the allow-list for a read-only sub-agent (see [`retain_only`]).
    ///
    /// [`retain_only`]: Self::retain_only
    pub fn read_only_names(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.is_read_only(n))
            .map(|n| n.to_string())
            .collect()
    }

    /// Whether `name`'s calls are safe to run concurrently (see
    /// [`Tool::concurrent`]); unknown names are not.
    pub fn is_concurrent(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.concurrent())
    }

    /// Execute a named tool. Errors from a missing tool are hard; errors from
    /// the tool body are returned to the caller to relay to the model.
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("unknown tool: {name}"))?;
        tool.execute(args, ctx).await
    }
}

/// Truncate `text` to `max` bytes on a char boundary, appending a marker that
/// tells the model output was cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = floor_char_boundary(text, max);
    let omitted = text.len() - end;
    format!(
        "{}\n\n… [output truncated, {omitted} bytes omitted]",
        &text[..end]
    )
}

/// Truncate to `max` bytes keeping the **head and tail** with the omission in
/// the middle. For shell output: build/test runs put the errors at the end, so
/// head-only truncation (plain [`truncate`]) would cut exactly what the model
/// needs. ~1/5 head, ~4/5 tail.
pub fn truncate_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let (head_end, tail_start) = middle_bounds(text, max);
    let omitted = tail_start - head_end;
    format!(
        "{}

… [{omitted} bytes omitted from the middle — the end of the output follows] …

{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// The head-end / tail-start byte offsets for a ~1/5-head, ~4/5-tail split at
/// `max` bytes (both on char boundaries). Shared by [`truncate_middle`] and
/// [`truncate_saved`].
fn middle_bounds(text: &str, max: usize) -> (usize, usize) {
    let head_target = max / 5;
    let tail_target = max - head_target;
    let head_end = floor_char_boundary(text, head_target);
    let mut tail_start = text.len() - tail_target;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    (head_end, tail_start)
}

/// Which end of the output to keep when truncating: `Head` (start; searches,
/// listings) or `Middle` (head + tail; shell output, where errors trail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncateSide {
    Head,
    Middle,
}

/// Directory holding full copies of truncated tool output. Under the system
/// temp dir on purpose: it's already read-whitelisted for the cwd-confined
/// `read`/`grep` tools, so the model can retrieve the overflow.
pub fn tool_output_dir() -> PathBuf {
    std::env::temp_dir().join("hrdr-tool-output")
}

/// Truncate `text` to `max_bytes` **and** `max_lines` (whichever is hit first,
/// matching opencode's `tool_output` limits), but instead of *discarding* the
/// overflow, write the **full** output to [`tool_output_dir`] and point the
/// model at it (so it can `read` a range or `grep` it rather than re-run
/// the tool). Falls back to a plain byte truncation if the file can't be
/// written. `label` tags the temp file (e.g. `"bash"`, `"grep"`).
pub fn truncate_saved(
    text: &str,
    max_bytes: usize,
    max_lines: usize,
    side: TruncateSide,
    label: &str,
) -> String {
    truncate_saved_in(text, max_bytes, max_lines, side, label, &tool_output_dir())
}

/// [`truncate_saved`] with an explicit overflow directory (for tests).
fn truncate_saved_in(
    text: &str,
    max_bytes: usize,
    max_lines: usize,
    side: TruncateSide,
    label: &str,
    dir: &std::path::Path,
) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    // Within both caps: hand it back untouched.
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return text.to_string();
    }
    let path = match save_overflow(dir, label, text) {
        Ok(p) => p,
        Err(_) => {
            // No file to point at — degrade to a plain byte truncation.
            return match side {
                TruncateSide::Head => truncate(text, max_bytes),
                TruncateSide::Middle => truncate_middle(text, max_bytes),
            };
        }
    };
    let hint = format!(
        "… [full output ({} lines, {} bytes) saved to {} — `read` it (with offset/limit) or \
         `grep` it (pattern + path) for the rest, don't re-run] …",
        lines.len(),
        text.len(),
        path.display()
    );
    match side {
        TruncateSide::Head => {
            let head = collect_lines(&lines, max_lines, max_bytes, false);
            format!("{head}\n\n{hint}")
        }
        // ~1/5 of each budget for the head, the rest for the tail (shell errors
        // trail), with the pointer bridging the gap.
        TruncateSide::Middle => {
            let head = collect_lines(&lines, max_lines / 5, max_bytes / 5, false);
            let tail = collect_lines(
                &lines,
                max_lines - max_lines / 5,
                max_bytes - max_bytes / 5,
                true,
            );
            format!("{head}\n\n{hint}\n\n{tail}")
        }
    }
}

/// Join whole lines from the head (or tail, when `from_tail`) of `lines`, up to
/// `max_lines` lines and `max_bytes` bytes — whichever caps first. At least one
/// line is always kept so the preview is never empty.
fn collect_lines(lines: &[&str], max_lines: usize, max_bytes: usize, from_tail: bool) -> String {
    let mut taken: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let ordered: Vec<&&str> = if from_tail {
        lines.iter().rev().collect()
    } else {
        lines.iter().collect()
    };
    for line in ordered {
        if taken.len() >= max_lines {
            break;
        }
        let add = line.len() + usize::from(!taken.is_empty()); // + the newline
        if bytes + add > max_bytes && !taken.is_empty() {
            break;
        }
        taken.push(line);
        bytes += add;
    }
    if from_tail {
        taken.reverse();
    }
    taken.join("\n")
}

/// Write `text` to a uniquely-named file under `dir` (created if needed),
/// returning the path. Best-effort prunes files older than 7 days first, so the
/// scratch dir can't grow without bound.
fn save_overflow(dir: &std::path::Path, label: &str, text: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(dir)?;
    prune_old_overflow(dir);

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}-{seq}.txt"));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Remove overflow files older than 7 days (best-effort; ignores all errors).
fn prune_old_overflow(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 60 * 60);
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Cap search output at `max_matches` *matches*, appending a count of what
/// was dropped and how to narrow the search. Only `path:NN:…` lines count as
/// matches — `path-NN-…` context lines and `--` group separators (grep/rg
/// `-C` format) ride along with their match, so a context-grep isn't
/// over-counted.
pub fn cap_matches(out: &str, max_matches: usize) -> String {
    let total = out.lines().filter(|l| is_match_line(l)).count();
    if total <= max_matches {
        return out.trim_end().to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut count = 0usize;
    for line in out.lines() {
        if is_match_line(line) {
            count += 1;
            if count > max_matches {
                break;
            }
        }
        kept.push(line);
    }
    let more = total - max_matches;
    format!(
        "{}\n… [{more} more matches — narrow the pattern or scope with path/glob]",
        kept.join("\n")
    )
}

/// Whether a search-output line is a match (`path:NN:…`) as opposed to a
/// `-C` context line (`path-NN-…`) or a `--` group separator.
fn is_match_line(line: &str) -> bool {
    let Some((_, rest)) = line.split_once(':') else {
        return false;
    };
    let Some((num, _)) = rest.split_once(':') else {
        return false;
    };
    !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit())
}

/// Collapse `s` to a single line (newlines → spaces) and truncate to `max`
/// **characters**, appending `…` if it was cut. For compact one-line previews
/// (tool-arg previews, status lines) — width-based, unlike the byte-based
/// [`truncate`].
pub fn truncate_inline(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Current Unix time in whole seconds (0 if the clock is before the epoch).
/// Shared by checkpoint journaling and session metadata.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Largest byte index `≤ max` that lies on a UTF-8 char boundary of `s`, so
/// `&s[..floor_char_boundary(s, max)]` never panics on multibyte text. Returns
/// `s.len()` when `max >= s.len()`. (std's `str::floor_char_boundary` is still
/// unstable, hence this helper.)
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // ---- secret-file deny-list ----

    #[test]
    fn secret_file_reason_matches_credential_patterns() {
        assert!(secret_file_reason(Path::new("/home/u/.config/hrdr/auth.toml")).is_some());
        assert!(secret_file_reason(Path::new("/home/u/.ssh/id_ed25519")).is_some());
        assert!(secret_file_reason(Path::new("/home/u/.aws/credentials")).is_some());
        assert!(secret_file_reason(Path::new("/home/u/.config/gh/hosts.yml")).is_some());
        assert!(secret_file_reason(Path::new("/srv/app/server.pem")).is_some());
        assert!(secret_file_reason(Path::new("/srv/app/tls.key")).is_some());
        assert!(secret_file_reason(Path::new("/srv/app/.env")).is_some());
        assert!(secret_file_reason(Path::new("/srv/app/.env.production")).is_some());
    }

    #[test]
    fn secret_file_reason_allows_normal_files() {
        assert!(secret_file_reason(Path::new("/srv/app/src/main.rs")).is_none());
        assert!(secret_file_reason(Path::new("/srv/app/README.md")).is_none());
        // A non-auth toml under a non-hrdr dir is fine.
        assert!(secret_file_reason(Path::new("/srv/app/Cargo.toml")).is_none());
        // `environment` is not a dotenv file.
        assert!(secret_file_reason(Path::new("/srv/app/environment")).is_none());
        // Non-secret dotenv templates stay readable (agents read these often).
        assert!(secret_file_reason(Path::new("/srv/app/.env.example")).is_none());
        assert!(secret_file_reason(Path::new("/srv/app/.env.sample")).is_none());
        assert!(secret_file_reason(Path::new("/srv/app/.env.template")).is_none());
    }

    // ---- concurrency defaults ----

    #[test]
    fn concurrent_defaults_to_read_only() {
        struct RoTool;
        #[async_trait::async_trait]
        impl Tool for RoTool {
            fn name(&self) -> &'static str {
                "ro"
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn read_only(&self) -> bool {
                true
            }
            async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> Result<String> {
                Ok(String::new())
            }
        }
        // A read-only tool is concurrent by default; a mutating one is not.
        assert!(RoTool.concurrent());
        assert!(!WriteTool.concurrent());
        assert!(!EditTool.concurrent());
    }

    // ---- tool scoping ----

    #[test]
    fn read_only_names_are_only_the_read_tools() {
        let r = ToolRegistry::with_defaults();
        let ro = r.read_only_names();
        // Read/search/web tools are read-only …
        assert!(ro.iter().any(|n| n == "read"));
        assert!(ro.iter().any(|n| n == "grep"));
        // … but the mutating ones never are.
        assert!(!ro.iter().any(|n| n == "write"));
        assert!(!ro.iter().any(|n| n == "edit"));
        assert!(!ro.iter().any(|n| n == "bash"));
    }

    #[test]
    fn retain_only_scopes_to_the_allow_list() {
        let mut r = ToolRegistry::with_defaults();
        r.retain_only(&["read".into(), "grep".into(), "nonexistent".into()]);
        let names: Vec<String> = r.defs().into_iter().map(|d| d.function.name).collect();
        assert_eq!(names, vec!["read".to_string(), "grep".to_string()]);
        assert!(!r.is_read_only("write")); // gone → unknown → not read-only
    }

    // ---- floor_char_boundary ----

    #[test]
    fn floor_char_boundary_never_splits_multibyte() {
        // "£" is 2 bytes (0xC2 0xA3). Byte index 1 is mid-codepoint.
        let s = "a£b"; // bytes: a(1) £(2) b(1) = 4 bytes
        assert_eq!(floor_char_boundary(s, 100), 4); // max ≥ len → len
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 2), 1); // byte 2 is mid-'£' → back to 1
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 0), 0);
        // The returned index is always safe to slice at.
        for max in 0..=s.len() + 2 {
            let end = floor_char_boundary(s, max);
            assert!(s.is_char_boundary(end));
            let _ = &s[..end]; // must not panic
        }
    }

    // ---- truncate ----

    #[test]
    fn truncate_under_max_returns_unchanged() {
        let text = "hello world";
        assert_eq!(truncate(text, 100), text);
    }

    #[test]
    fn truncate_exact_max_returns_unchanged() {
        // text.len() == max is the boundary; no marker should be added.
        let text = "abcde";
        assert_eq!(truncate(text, 5), text);
    }

    #[test]
    fn truncate_over_max_adds_marker() {
        let text = "abcdefghij"; // 10 bytes
        let out = truncate(text, 5);
        assert!(out.starts_with("abcde"), "prefix wrong: {out}");
        assert!(out.contains("[output truncated"), "marker missing: {out}");
        assert!(out.contains("5 bytes omitted"), "byte count missing: {out}");
    }

    #[test]
    fn truncate_does_not_split_multibyte_char() {
        // '£' is U+00A3, encoded as 0xC2 0xA3 (2 bytes in UTF-8).
        // "££££" = 8 bytes. Setting max = 3 would land mid-codepoint at byte 3;
        // the implementation must back up to byte 2 (the only char boundary ≤ 3).
        let text = "££££";
        assert_eq!(text.len(), 8);
        let out = truncate(text, 3);
        // Output must be valid UTF-8 (no panic or replacement bytes).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        // The prefix must start with exactly one '£' (2 bytes kept).
        assert!(
            out.starts_with('£'),
            "expected at least one '£' in output: {out}"
        );
        // Must contain the truncation marker.
        assert!(out.contains("[output truncated"), "marker missing: {out}");
    }

    // ---- ToolContext::resolve ----

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let text = format!(
            "HEAD-MARKER
{}
TAIL-ERROR-LINE",
            "x".repeat(50_000)
        );
        let out = truncate_middle(&text, 10_000);
        assert!(out.starts_with("HEAD-MARKER"));
        assert!(out.ends_with("TAIL-ERROR-LINE"), "tail must survive");
        assert!(out.contains("bytes omitted from the middle"));
        assert!(out.len() < 11_000);
        // Under the cap: untouched.
        assert_eq!(truncate_middle("short", 100), "short");
    }

    #[test]
    fn truncate_saved_persists_overflow_and_points_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("HEAD\n{}\nTAIL", "x".repeat(50_000));

        // Head mode: keeps the start, saves the full output, points at the file.
        // Generous line cap so the byte cap is what bites here.
        let out = truncate_saved_in(
            &text,
            10_000,
            100_000,
            TruncateSide::Head,
            "grep",
            dir.path(),
        );
        assert!(out.starts_with("HEAD"));
        assert!(out.contains("full output"));
        assert!(out.contains("read"));
        // Exactly one overflow file, containing the FULL text verbatim.
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read_to_string(files[0].path()).unwrap(), text);
        // The saved path is named after the label and referenced in the output.
        let name = files[0].file_name().to_string_lossy().into_owned();
        assert!(name.starts_with("grep-"));
        assert!(out.contains(&files[0].path().display().to_string()));

        // Middle mode keeps head and tail around the pointer.
        let mid = truncate_saved_in(
            &text,
            10_000,
            100_000,
            TruncateSide::Middle,
            "bash",
            dir.path(),
        );
        assert!(mid.starts_with("HEAD"));
        assert!(mid.trim_end().ends_with("TAIL"), "tail must survive");
    }

    #[test]
    fn truncate_saved_caps_on_lines_too() {
        let dir = tempfile::tempdir().unwrap();
        // 5000 short lines: well under any byte cap, but over the line cap.
        let text = (0..5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_saved_in(
            &text,
            10_000_000,
            2000,
            TruncateSide::Head,
            "grep",
            dir.path(),
        );
        // Truncated by lines (kept the head), full copy saved, pointer present.
        assert!(out.starts_with("line 0"));
        assert!(out.contains("5000 lines"));
        assert!(out.lines().count() <= 2000 + 3); // preview + hint lines
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn truncate_saved_leaves_small_output_untouched() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            truncate_saved_in("short", 100, 100, TruncateSide::Head, "grep", dir.path()),
            "short"
        );
        // No file written when nothing overflowed.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn cap_matches_limits_and_counts() {
        let out: String = (0..300)
            .map(|i| {
                format!(
                    "f.rs:{i}:hit
"
                )
            })
            .collect();
        let capped = cap_matches(&out, 200);
        assert_eq!(capped.lines().count(), 201); // 200 matches + marker
        assert!(
            capped.ends_with("[100 more matches — narrow the pattern or scope with path/glob]")
        );
        // Under the cap: untouched (minus trailing newline).
        assert_eq!(
            cap_matches(
                "a:1:x
b:2:y
",
                200
            ),
            "a:1:x
b:2:y"
        );
    }

    #[test]
    fn cap_matches_ignores_context_lines_and_separators() {
        // Context lines (dash format) and `--` separators don't count as
        // matches; each kept match keeps its surrounding context.
        let ctx_out =
            "f.rs-1-a\nf.rs:2:hit\nf.rs-3-b\n--\nf.rs-9-c\nf.rs:10:hit\n--\nf.rs:20:hit\n";
        let capped = cap_matches(ctx_out, 2);
        assert!(capped.contains("f.rs:2:hit") && capped.contains("f.rs:10:hit"));
        assert!(!capped.contains("f.rs:20:hit"));
        assert!(capped.contains("[1 more matches"));
        assert!(
            capped.contains("f.rs-9-c"),
            "context of kept match survives"
        );
        // Untouched when matches (not lines) are under the cap.
        assert_eq!(cap_matches(ctx_out, 3), ctx_out.trim_end());
    }

    #[test]
    fn ensure_within_cwd_confines_writes() {
        // NB: tempdirs live under the system temp dir, which the gate always
        // allows for scratch — so "outside" fixtures must be non-temp paths.
        // The gate is a pure check (it fires before any I/O), so the paths
        // don't need to exist or be writable.
        let mut ctx = ToolContext::new("/etc");
        // Inside cwd (including not-yet-created nested paths): allowed.
        assert!(ctx.ensure_within_cwd(Path::new("/etc/sub/new.txt")).is_ok());
        // Outside cwd: refused, with the recovery in the message.
        let err = ctx
            .ensure_within_cwd(Path::new("/usr/lib/x.txt"))
            .unwrap_err();
        assert!(err.to_string().contains("outside the working directory"));
        // `..` escapes are resolved before the check.
        assert!(
            ctx.ensure_within_cwd(Path::new("/etc/../usr/escape.txt"))
                .is_err()
        );
        // The system temp dir is always fair game for scratch…
        assert!(
            ctx.ensure_within_cwd(&std::env::temp_dir().join("hrdr-scratch.txt"))
                .is_ok()
        );
        // …a temp cwd is inside by definition…
        let dir = tempfile::tempdir().unwrap();
        let tmp_ctx = ToolContext::new(dir.path());
        assert!(tmp_ctx.ensure_within_cwd(&dir.path().join("a.txt")).is_ok());
        // …and the knob disables the gate entirely.
        ctx.restrict_to_cwd = false;
        assert!(ctx.ensure_within_cwd(Path::new("/usr/lib/x.txt")).is_ok());
    }

    #[test]
    fn write_allow_ext_confines_mutations_to_listed_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path());
        ctx.write_allow_ext = Some(vec!["md".into(), "markdown".into()]);
        // Listed extensions pass (case-insensitive)…
        assert!(ctx.ensure_within_cwd(&dir.path().join("PLAN.md")).is_ok());
        assert!(
            ctx.ensure_within_cwd(&dir.path().join("a.MARKDOWN"))
                .is_ok()
        );
        // …anything else is refused, even inside cwd.
        let err = ctx
            .ensure_within_cwd(&dir.path().join("src/main.rs"))
            .unwrap_err();
        assert!(err.to_string().contains("only modify"), "{err}");
        // Extensionless paths aren't in the list → refused.
        assert!(ctx.ensure_within_cwd(&dir.path().join("Makefile")).is_err());
        // No list → no restriction.
        ctx.write_allow_ext = None;
        assert!(
            ctx.ensure_within_cwd(&dir.path().join("src/main.rs"))
                .is_ok()
        );
    }

    #[test]
    fn tool_context_resolve_absolute_path() {
        let ctx = ToolContext::new("/some/cwd");
        let abs = "/absolute/path/file.txt";
        assert_eq!(ctx.resolve(abs), PathBuf::from(abs));
    }

    #[test]
    fn tool_context_resolve_relative_path() {
        let ctx = ToolContext::new("/my/cwd");
        assert_eq!(
            ctx.resolve("sub/file.txt"),
            PathBuf::from("/my/cwd/sub/file.txt")
        );
    }
}
