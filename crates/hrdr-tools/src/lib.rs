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

mod tools;
mod web;

pub use tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, TodoTool, WriteTool};
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
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            todos: Arc::new(Mutex::new(Vec::new())),
            max_output: DEFAULT_MAX_OUTPUT,
            stream: None,
        }
    }

    /// Send a chunk of live output to the streaming sink, if one is attached.
    pub fn emit(&self, chunk: impl Into<String>) {
        if let Some(tx) = &self.stream {
            let _ = tx.send(chunk.into());
        }
    }

    /// Resolve a possibly-relative path against `cwd`.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() { p } else { self.cwd.join(p) }
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

    /// The default set: Core 6 + `todo_write` + web (`web_fetch`, `web_search`).
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WriteTool));
        r.register(Arc::new(EditTool));
        r.register(Arc::new(BashTool));
        r.register(Arc::new(GrepTool));
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

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
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
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = text.len() - end;
    format!(
        "{}\n\n… [output truncated, {omitted} bytes omitted]",
        &text[..end]
    )
}
