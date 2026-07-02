//! `hrdr-tools` — the agentic tool set.
//!
//! The locked MVP set: `read_file`, `write_file`, `edit`, `bash`, `grep`,
//! `glob`, `todo_write`. Each implements [`Tool`] and is exposed to the model
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
mod tools;
mod web;

pub use checkpoint::{CheckpointInfo, Checkpoints};
pub use guardrails::{Guardrail, check_guardrails, default_guardrails};
pub use tools::{
    BashTool, EditTool, GlobTool, GrepTool, PowerShellTool, ReadTool, TodoTool, WriteTool,
    available_shell_tools,
};
pub use web::{WebFetchTool, WebSearchTool};

/// Default cap on a single tool's textual output, in bytes. Larger results are
/// truncated with a marker so the model's context is never blown by one call.
pub const DEFAULT_MAX_OUTPUT: usize = 30_000;

/// A single TODO item tracked by `todo_write`.
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

/// Shared execution context handed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    /// Working directory tool paths resolve against.
    pub cwd: PathBuf,
    /// Shared TODO list, mutated by `todo_write`, surfaced to the UI.
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
    /// Per-call output byte cap.
    pub max_output: usize,
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
    /// this session). `edit`/`write_file` refuse to mutate an existing file
    /// that isn't here — blind edits against guessed content are the top
    /// source of corrupt patches.
    pub read_files: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            todos: Arc::new(Mutex::new(Vec::new())),
            max_output: DEFAULT_MAX_OUTPUT,
            stream: None,
            checkpoints: None,
            guardrails: Arc::new(default_guardrails()),
            read_files: Arc::new(Mutex::new(std::collections::HashSet::new())),
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

    /// Whether the model has seen `path`'s current content this session.
    pub fn was_read(&self, path: &std::path::Path) -> bool {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.read_files
            .lock()
            .map(|set| set.contains(&canon))
            .unwrap_or(true) // poisoned lock: don't wedge edits
    }
}

/// A model-callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the call arguments.
    fn parameters(&self) -> serde_json::Value;

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
        // Shell tools are presence-gated so the model is only offered a shell it
        // can actually use (bash on unix; PowerShell where installed, incl. Linux).
        for shell in available_shell_tools() {
            r.register(shell);
        }
        r.register(Arc::new(GrepTool::detect()));
        r.register(Arc::new(GlobTool));
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
    use std::path::PathBuf;

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
