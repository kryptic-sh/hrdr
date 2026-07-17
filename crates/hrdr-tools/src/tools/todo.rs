use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{TodoItem, Tool, ToolContext};

// ---- todo ----

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "Replace the task list for the current work. Use it to plan and track multi-step \
         coding tasks: mark exactly one item `in_progress`, the rest \
         `pending`/`completed`/`cancelled`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full task list, replacing whatever was there before.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "The task, in a few words."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "pending: not started. in_progress: exactly one item at a time. completed: done. cancelled: abandoned."}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let items = parse_todos(args).context("invalid todo args")?;
        let rendered = render_todos(&items);
        // A poisoned lock must not silently report success with a stale list.
        *ctx.todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = items;
        Ok(rendered)
    }
}

/// Forgivingly extract the todo list from `todo` arguments. The schema is
/// the standard `{"todos": [{content, status}, …]}`, but smaller models often
/// echo the JSON-Schema shape into the value or drop/rename the wrapper, so we
/// also accept `{"todos": {"items": […]}}` (the schema-echo mistake), a bare
/// `{"items": […]}` / `{"tasks": […]}`, and a top-level array.
pub(crate) fn parse_todos(args: serde_json::Value) -> Result<Vec<TodoItem>> {
    let arr = match args {
        Value::Array(a) => a,
        Value::Object(mut m) => {
            let v = m
                .remove("todos")
                .or_else(|| m.remove("items"))
                .or_else(|| m.remove("tasks"))
                .ok_or_else(|| anyhow!("expected a `todos` array of {{content, status}} items"))?;
            match v {
                Value::Array(a) => a,
                // `{"todos": {"items": […]}}` — the model copied the schema's
                // `items` keyword instead of emitting a bare array.
                Value::Object(mut inner) => {
                    match inner.remove("items").or_else(|| inner.remove("todos")) {
                        Some(Value::Array(a)) => a,
                        _ => bail!("`todos` must be an array of {{content, status}} items"),
                    }
                }
                // A single item object instead of a one-element array.
                other => vec![other],
            }
        }
        _ => bail!("expected an object with a `todos` array"),
    };
    arr.into_iter().map(parse_item).collect()
}

/// Parse one todo item, tolerating `task`/`text`/`title` aliases for the content
/// and a range of status spellings (see [`normalize_status`]).
fn parse_item(v: serde_json::Value) -> Result<TodoItem> {
    let Value::Object(mut m) = v else {
        bail!("each todo must be an object with a `content` string");
    };
    let content = m
        .remove("content")
        .or_else(|| m.remove("task"))
        .or_else(|| m.remove("text"))
        .or_else(|| m.remove("title"))
        .and_then(|c| match c {
            Value::String(s) => Some(s),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("each todo needs a non-empty `content` string"))?;
    let status = m
        .remove("status")
        .or_else(|| m.remove("state"))
        .and_then(|s| s.as_str().map(normalize_status))
        .unwrap_or_else(|| "pending".to_string());
    Ok(TodoItem { content, status })
}

/// Map a free-form status string onto one of `pending | in_progress | completed | cancelled`.
/// Unknown values fall back to `pending`, so a bad status never fails the call.
fn normalize_status(s: &str) -> String {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "completed" | "complete" | "done" | "finished" | "x" | "[x]" => "completed",
        "in_progress" | "inprogress" | "doing" | "active" | "current" | "wip" | "started"
        | "ongoing" => "in_progress",
        "cancelled" | "canceled" | "canceling" | "cancelling" | "abandoned" | "skipped"
        | "removed" | "stale" => "cancelled",
        _ => "pending",
    }
    .to_string()
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(todo list cleared)".to_string();
    }
    let mut out = String::new();
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "✓",
            "cancelled" => "✗",
            "in_progress" => "⠋",
            _ => " ",
        };
        out.push_str(&format!("{mark} {}\n", t.content));
    }
    out
}
