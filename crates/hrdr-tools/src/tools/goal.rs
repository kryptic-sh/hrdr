use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::json;

use crate::{GoalItem, Tool, ToolContext};

// ---- goal ----

/// A longer-horizon objective the model sets for itself and is reminded of at
/// turn end while it stays pending. Where `todo` is the replace-whole-list
/// task tracker, `goal` is a small set of standing intentions with three
/// operations: add one, cancel one (the model cancels a goal it considers
/// achieved or abandoned — that explicit cancel is what the turn-end nudge
/// asks for), or list them.
pub struct GoalTool;

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &'static str {
        "goal"
    }
    fn description(&self) -> &'static str {
        "Track long-horizon goals the model set for itself. Unlike `todo` (the current task \
         list), a goal is a standing intention: the harness reminds you of pending goals when \
         you try to end a turn without having resolved them. Use `goal` with `op: add` (a \
         `content` string), `op: cancel` (the `id` of a goal that is already achieved or \
         abandoned — say why), or `op: list`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["add", "cancel", "list"],
                    "description": "add: create a goal with `content`. cancel: mark the goal \
                                    with `id` cancelled (achieved or abandoned — say why). \
                                    list: show all goals."
                },
                "content": {
                    "type": "string",
                    "default": "",
                    "description": "The goal, for `op: add`."
                },
                "id": {
                    "type": "integer",
                    "default": 0,
                    "description": "The goal's `#N` id, for `op: cancel`. Ids come from the \
                                    `list` output or the add result."
                }
            },
            "required": ["op"]
        })
    }
    /// `read_only` here means what it means everywhere else in the registry:
    /// *does not mutate the working tree*. `goal` mutates a `Vec<GoalItem>`
    /// behind a mutex in the agent's own [`ToolContext`] — no file, no process,
    /// nothing outside this agent's memory, exactly like `todo`.
    fn read_only(&self) -> bool {
        true
    }
    /// …but opt back out of concurrency, which `read_only` would otherwise
    /// imply. `add` and `cancel` both mutate the same shared list, so two of
    /// them in one batch are order-sensitive — sequential keeps "the last call
    /// the model made is the list it gets", which is what the turn-end goal
    /// check then reads back.
    fn concurrent(&self) -> bool {
        false
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GoalArgs = crate::tool_args("goal", args)?;
        match a.op.as_str() {
            "add" => {
                let content = a
                    .content
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("`goal` with `op: add` needs a non-empty `content`"))?;
                let mut goals = ctx
                    .goals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let id = goals.iter().map(|g| g.id).max().unwrap_or(0) + 1;
                goals.push(GoalItem {
                    content: content.trim().to_string(),
                    id,
                    status: "pending".to_string(),
                });
                Ok(format!(
                    "goal #{id} set: {}\n\n{}",
                    goals.last().unwrap().content,
                    render_goals(&goals)
                ))
            }
            "cancel" => {
                let id =
                    a.id.ok_or_else(|| anyhow!("`goal` with `op: cancel` needs an `id`"))?;
                let mut goals = ctx
                    .goals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(goal) = goals.iter_mut().find(|g| g.id == id) else {
                    bail!("no goal #{id}. Ids come from `goal`'s list output or an add result.");
                };
                if goal.status == "cancelled" {
                    bail!("goal #{id} is already cancelled.");
                }
                goal.status = "cancelled".to_string();
                let content = goal.content.clone();
                let rendered = render_goals(&goals);
                Ok(format!("goal #{id} cancelled: {content}\n\n{rendered}"))
            }
            "list" => {
                let goals = ctx
                    .goals
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Ok(render_goals(&goals))
            }
            other => bail!("unknown goal op `{other}` — use `add`, `cancel`, or `list`"),
        }
    }
}

#[derive(serde::Deserialize)]
struct GoalArgs {
    op: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    id: Option<u64>,
}

/// Render the goal list the way `todo` renders its list — `#N` + status mark +
/// content, pending first — so the model reads ids back for a cancel.
fn render_goals(goals: &[GoalItem]) -> String {
    if goals.is_empty() {
        return "(no goals)".to_string();
    }
    let mut out = String::new();
    for g in goals {
        let mark = if g.status == "cancelled" {
            "✗"
        } else {
            "○"
        };
        out.push_str(&format!("#{} {mark} {}\n", g.id, g.content));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> GoalTool {
        GoalTool
    }

    #[tokio::test]
    async fn add_mints_increasing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = tool()
            .execute(json!({"op": "add", "content": "ship the release"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("#1"), "{out}");
        let out = tool()
            .execute(json!({"op": "add", "content": "fix the CI"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("#2"), "{out}");
        let goals = ctx.goals.lock().unwrap();
        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0].id, 1);
        assert_eq!(goals[1].id, 2);
        assert!(goals.iter().all(|g| g.status == "pending"));
    }

    #[tokio::test]
    async fn add_requires_content() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = tool()
            .execute(json!({"op": "add"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("content"), "{err}");
    }

    #[tokio::test]
    async fn cancel_marks_the_named_goal() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        tool()
            .execute(json!({"op": "add", "content": "a"}), &ctx)
            .await
            .unwrap();
        tool()
            .execute(json!({"op": "add", "content": "b"}), &ctx)
            .await
            .unwrap();
        let out = tool()
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("goal #1 cancelled"), "{out}");
        let goals = ctx.goals.lock().unwrap();
        assert_eq!(goals[0].status, "cancelled");
        assert_eq!(goals[1].status, "pending");
    }

    #[tokio::test]
    async fn cancel_of_unknown_or_double_cancelled_goal_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let err = tool()
            .execute(json!({"op": "cancel", "id": 9}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no goal #9"), "{err}");
        tool()
            .execute(json!({"op": "add", "content": "a"}), &ctx)
            .await
            .unwrap();
        tool()
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap();
        let err = tool()
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already cancelled"), "{err}");
    }

    #[tokio::test]
    async fn list_renders_ids_and_marks() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path());
        let out = tool().execute(json!({"op": "list"}), &ctx).await.unwrap();
        assert!(out.contains("(no goals)"), "{out}");
        tool()
            .execute(json!({"op": "add", "content": "a"}), &ctx)
            .await
            .unwrap();
        tool()
            .execute(json!({"op": "add", "content": "b"}), &ctx)
            .await
            .unwrap();
        tool()
            .execute(json!({"op": "cancel", "id": 1}), &ctx)
            .await
            .unwrap();
        let out = tool().execute(json!({"op": "list"}), &ctx).await.unwrap();
        assert!(out.contains("#1 ✗ a"), "{out}");
        assert!(out.contains("#2 ○ b"), "{out}");
    }
}
